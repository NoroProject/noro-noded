// Файл превышает 150 строк: выгрузка архива, потоковая выгрузка снимка частями и скачивание обратно — одна связь с хранилищем.
//! Выгрузка и скачивание бэкапов во внешние хранилища напрямую с ноды.
//!
//! Нода держит бэкап на диске и сама отправляет его в S3/R2/MinIO, минуя
//! мастер: гонять тяжёлый тарбол через мастер-сервер бессмысленно.
//!
//! В хранилище бэкап всегда лежит собранным `.tar.gz` — и полный архив, и
//! снимок из кусков. Хранилище — это то, из чего восстанавливают, когда ноды
//! уже нет: архив там обязан открываться сам по себе, без нашего формата и
//! без соседних объектов. Куски с дедупликацией в хранилище экономили бы
//! место, но тогда удаление бэкапа превращается в сборку мусора по чужому
//! бакету, а мастер, удаляющий объект бэкапа сам, этого не умеет.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Method};
use schema::noded::BackupUpload;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::s3::{self, S3Config, EMPTY_SHA256};

/// Сколько ждать хранилище, если оно не отвечает вовсе.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Часть потоковой выгрузки. S3 принимает не больше десяти тысяч частей:
/// 32 МБ — это потолок в 320 ГБ на бэкап при 32 МБ памяти на выгрузку.
const PART: usize = 32 * 1024 * 1024;

/// Потолок на всю передачу: пятнадцать минут плюс секунда на каждый мегабайт.
///
/// Ровные пятнадцать минут прежде обрывали заливку мира на двадцать гигабайт
/// на середине, а без потолка зависшее хранилище держало бы задачу вечно.
fn transfer_timeout(bytes: u64) -> std::time::Duration {
    std::time::Duration::from_secs(900 + bytes / (1024 * 1024))
}

fn client(bytes: u64) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(transfer_timeout(bytes))
        .build()?)
}

fn s3_config(target: &BackupUpload) -> Result<S3Config> {
    if target.kind != "s3" {
        bail!("хранилище типа '{}' не поддерживается на ноде", target.kind);
    }
    serde_json::from_value(target.config.clone()).context("неверная конфигурация S3 хранилища")
}

/// Залить готовый архив во внешнее хранилище.
///
/// Архив читается потоком: прежде он целиком ложился в память демона, и бэкап
/// большого мира заканчивался тем, что ядро убивало демон вместе с консолями
/// всех серверов ноды.
pub async fn upload(archive: &Path, target: &BackupUpload) -> Result<()> {
    let cfg = s3_config(target)?;
    let key = cfg.full_key(&target.path);

    // Архив открывается мимо ссылок. Подпись S3 требует хеш тела заранее: он
    // считается проходом по тому же открытому файлу, а не по пути.
    let path = archive.to_path_buf();
    let (file, size, content_sha256) = tokio::task::spawn_blocking(move || -> Result<_> {
        use std::io::{Seek, SeekFrom};
        let mut file = crate::fs::nofollow::open_existing(&path, false)
            .context("не открыть локальный архив бэкапа")?;
        let mut hasher = Sha256::new();
        let size = std::io::copy(&mut file, &mut hasher)?;
        file.seek(SeekFrom::Start(0))?;
        Ok((file, size, hex::encode(hasher.finalize())))
    })
    .await
    .map_err(|e| anyhow::anyhow!("чтение архива не выполнено: {e}"))??;

    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
    let req = s3::Request {
        method: Method::PUT,
        key: &key,
        query: vec![],
        headers: vec![("content-type", "application/octet-stream".into())],
        payload_sha256: content_sha256,
    };
    s3::send(
        req.build(&client(size)?, &cfg)
            .header("content-length", size)
            .body(reqwest::Body::wrap_stream(stream)),
        "PUT",
    )
    .await?;
    Ok(())
}

/// Залить снимок собранным tar.gz — по частям, по мере сборки.
///
/// Длины архива заранее не знает никто, а обычная заливка требует и её, и
/// хеш всего тела. Собирать его ради этого дважды — вдвое дольше, во
/// временный файл — ещё один мир на диске. Многочастная заливка подписывает
/// каждую часть отдельно, и в памяти лежит только текущая.
pub async fn upload_snapshot(
    repo: PathBuf,
    backup: uuid::Uuid,
    target: &BackupUpload,
) -> Result<()> {
    let cfg = s3_config(target)?;
    let key = cfg.full_key(&target.path);
    let http = client(PART as u64)?;

    let created = s3::send(
        s3::Request {
            method: Method::POST,
            key: &key,
            query: vec![("uploads", String::new())],
            headers: vec![("content-type", "application/gzip".into())],
            payload_sha256: EMPTY_SHA256.into(),
        }
        .build(&http, &cfg),
        "CreateMultipartUpload",
    )
    .await?
    .text()
    .await?;
    let upload_id = xml_value(&created, "UploadId").context("хранилище не выдало UploadId")?;

    let sent = send_parts(&http, &cfg, &key, &upload_id, repo, backup).await;
    let completed = match sent {
        Ok(etags) => complete(&http, &cfg, &key, &upload_id, &etags).await,
        Err(e) => Err(e),
    };
    if completed.is_err() {
        // Брошенная многочастная заливка лежит в бакете невидимо и стоит
        // денег, пока её не отменят.
        let abort = s3::Request {
            method: Method::DELETE,
            key: &key,
            query: vec![("uploadId", upload_id.clone())],
            headers: vec![],
            payload_sha256: EMPTY_SHA256.into(),
        };
        let _ = s3::send(abort.build(&http, &cfg), "AbortMultipartUpload").await;
    }
    completed
}

async fn send_parts(
    http: &Client,
    cfg: &S3Config,
    key: &str,
    upload_id: &str,
    repo: PathBuf,
    backup: uuid::Uuid,
) -> Result<Vec<String>> {
    let mut stream = Box::pin(super::stream::tar_gz_stream(repo, backup));
    let mut etags = Vec::new();
    let mut part = Vec::with_capacity(PART);
    let mut done = false;
    while !done {
        match stream.next().await {
            Some(bytes) => part.extend_from_slice(&bytes?),
            None => done = true,
        }
        // Последняя часть может быть любой длины, в том числе единственная.
        if part.len() < PART && !(done && (!part.is_empty() || etags.is_empty())) {
            continue;
        }
        let body = std::mem::replace(&mut part, Vec::with_capacity(PART));
        let number = etags.len() + 1;
        let req = s3::Request {
            method: Method::PUT,
            key,
            query: vec![
                ("partNumber", number.to_string()),
                ("uploadId", upload_id.to_string()),
            ],
            headers: vec![],
            payload_sha256: hex::encode(Sha256::digest(&body)),
        };
        let res = s3::send(
            req.build(http, cfg)
                .header("content-length", body.len())
                .body(body),
            "UploadPart",
        )
        .await?;
        let etag = res
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .context("хранилище не вернуло ETag части")?;
        etags.push(etag.to_string());
    }
    Ok(etags)
}

async fn complete(
    http: &Client,
    cfg: &S3Config,
    key: &str,
    upload_id: &str,
    etags: &[String],
) -> Result<()> {
    let mut body = String::from("<CompleteMultipartUpload>");
    for (i, etag) in etags.iter().enumerate() {
        body.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{etag}</ETag></Part>",
            i + 1
        ));
    }
    body.push_str("</CompleteMultipartUpload>");
    let req = s3::Request {
        method: Method::POST,
        key,
        query: vec![("uploadId", upload_id.to_string())],
        headers: vec![("content-type", "application/xml".into())],
        payload_sha256: hex::encode(Sha256::digest(body.as_bytes())),
    };
    let text = s3::send(req.build(http, cfg).body(body), "CompleteMultipartUpload")
        .await?
        .text()
        .await?;
    // S3 умеет ответить 200 и ошибкой в теле, если сборка частей сорвалась
    // уже после того, как ответ начался.
    if text.contains("<Error>") {
        bail!("S3 CompleteMultipartUpload: {text}");
    }
    Ok(())
}

/// Значение первого тега `<name>` в ответе S3.
fn xml_value(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let len = xml[start..].find(&format!("</{name}>"))?;
    Some(xml[start..start + len].to_string())
}

/// Скачать архив из внешнего хранилища перед восстановлением.
///
/// Потоком во временный файл рядом и переименованием: целиком в памяти он не
/// помещается, а оборванное скачивание не должно оставлять огрызок под именем
/// настоящего архива.
pub async fn download(archive: &Path, source: &BackupUpload) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let cfg = s3_config(source)?;
    let key = cfg.full_key(&source.path);

    let (Some(parent), Some(name)) = (archive.parent(), archive.file_name()) else {
        bail!("у пути архива нет имени");
    };
    tokio::fs::create_dir_all(parent).await?;
    // Мимо ссылок, как всякая запись демона в каталог данных.
    let dir = crate::fs::nofollow::Dir::open(parent)?;
    let tmp = {
        let mut tmp = std::ffi::OsString::from(".");
        tmp.push(name);
        tmp.push(format!(".noro-part-{}", uuid::Uuid::new_v4().simple()));
        dir.entry(tmp)
    };

    // Размер заранее неизвестен — потолок по самому большому разумному миру.
    let req = s3::Request {
        method: Method::GET,
        key: &key,
        query: vec![],
        headers: vec![],
        payload_sha256: EMPTY_SHA256.into(),
    };
    let response = s3::send(req.build(&client(64 * 1024 * 1024 * 1024)?, &cfg), "GET").await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    let written = async {
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        file.sync_all().await?;
        anyhow::Ok(())
    }
    .await;
    drop(file);
    let renamed = match written {
        Ok(()) => tokio::fs::rename(&tmp, dir.entry(name))
            .await
            .map_err(anyhow::Error::from),
        Err(e) => Err(e),
    };
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    renamed.context("ошибка скачивания из S3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upload_id_is_read_from_the_reply() {
        let xml = r#"<?xml version="1.0"?><InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>VXBsb2Fk.ID</UploadId></InitiateMultipartUploadResult>"#;
        assert_eq!(xml_value(xml, "UploadId").as_deref(), Some("VXBsb2Fk.ID"));
        assert_eq!(xml_value(xml, "Missing"), None);
    }
}
