// Файл превышает 150 строк: клиент S3-совместимого хранилища для выгрузки и скачивания бэкапов на ноде.
//! Выгрузка и скачивание бэкапов во внешние хранилища напрямую с ноды.
//!
//! Нода держит готовый архив на диске и сама отправляет его в S3/R2/MinIO,
//! минуя мастер: гонять тяжёлый тарбол через мастер-сервер бессмысленно.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::Client;
use schema::noded::BackupUpload;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize)]
struct S3Config {
    #[serde(default = "default_endpoint")]
    endpoint: String,
    #[serde(default = "default_region")]
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    #[serde(default)]
    path_prefix: String,
}

fn default_endpoint() -> String {
    "https://s3.amazonaws.com".into()
}

fn default_region() -> String {
    "us-east-1".into()
}

impl S3Config {
    fn full_key(&self, filename: &str) -> String {
        let prefix = self.path_prefix.trim().trim_matches('/');
        if prefix.is_empty() {
            filename.trim_start_matches('/').to_string()
        } else {
            format!("{prefix}/{}", filename.trim_start_matches('/'))
        }
    }
}

/// Залить готовый архив во внешнее хранилище.
pub async fn upload(archive: &Path, target: &BackupUpload) -> Result<()> {
    if target.kind != "s3" {
        bail!("хранилище типа '{}' не поддерживается на ноде", target.kind);
    }
    let cfg: S3Config = serde_json::from_value(target.config.clone())
        .context("неверная конфигурация S3 хранилища")?;
    let key = cfg.full_key(&target.path);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .build()?;
    let data = tokio::fs::read(archive)
        .await
        .context("не прочитать локальный архив бэкапа")?;

    s3_put(&client, &cfg, &key, &data).await
}

/// Скачать архив из внешнего хранилища перед восстановлением.
pub async fn download(archive: &Path, source: &BackupUpload) -> Result<()> {
    if source.kind != "s3" {
        bail!("хранилище типа '{}' не поддерживается на ноде", source.kind);
    }
    let cfg: S3Config = serde_json::from_value(source.config.clone())
        .context("неверная конфигурация S3 хранилища")?;
    let key = cfg.full_key(&source.path);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .build()?;
    let bytes = s3_get(&client, &cfg, &key).await?;

    if let Some(parent) = archive.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(archive, bytes).await?;
    Ok(())
}

async fn s3_put(client: &Client, cfg: &S3Config, key: &str, data: &[u8]) -> Result<()> {
    let now = Utc::now();
    let date = now.format("%Y%m%d").to_string();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();

    let host = endpoint_host(&cfg.endpoint);
    let path = format!("/{}/{}", cfg.bucket, key.trim_start_matches('/'));
    let content_sha256 = hex::encode(Sha256::digest(data));
    let content_type = "application/octet-stream";

    let canonical_headers = format!(
        "content-type:{content_type}\nhost:{host}\nx-amz-content-sha256:{content_sha256}\nx-amz-date:{datetime}\n"
    );
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";

    let canonical_request =
        format!("PUT\n{path}\n\n{canonical_headers}\n{signed_headers}\n{content_sha256}");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{date}/{}/{}/aws4_request\n{}",
        cfg.region,
        "s3",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let signing_key = derive_signing_key(&cfg.secret_key, &date, &cfg.region);
    let mut mac = HmacSha256::new_from_slice(&signing_key).unwrap();
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request,SignedHeaders={signed_headers},Signature={signature}",
        cfg.access_key, date, cfg.region
    );

    let url = format!("{}/{}", cfg.endpoint.trim_end_matches('/'), &path[1..]);
    let res = client
        .put(&url)
        .header("host", &host)
        .header("content-type", content_type)
        .header("x-amz-content-sha256", &content_sha256)
        .header("x-amz-date", &datetime)
        .header("authorization", &authorization)
        .body(data.to_vec())
        .send()
        .await
        .context("ошибка отправки запроса в S3")?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("S3 PUT ошибка {status}: {body}");
    }
    Ok(())
}

async fn s3_get(client: &Client, cfg: &S3Config, key: &str) -> Result<Vec<u8>> {
    let now = Utc::now();
    let date = now.format("%Y%m%d").to_string();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();

    let host = endpoint_host(&cfg.endpoint);
    let path = format!("/{}/{}", cfg.bucket, key.trim_start_matches('/'));
    let content_sha256 = hex::encode(Sha256::digest(b""));

    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{content_sha256}\nx-amz-date:{datetime}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request =
        format!("GET\n{path}\n\n{canonical_headers}\n{signed_headers}\n{content_sha256}");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{date}/{}/{}/aws4_request\n{}",
        cfg.region,
        "s3",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let signing_key = derive_signing_key(&cfg.secret_key, &date, &cfg.region);
    let mut mac = HmacSha256::new_from_slice(&signing_key).unwrap();
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request,SignedHeaders={signed_headers},Signature={signature}",
        cfg.access_key, date, cfg.region
    );

    let url = format!("{}/{}", cfg.endpoint.trim_end_matches('/'), &path[1..]);
    let res = client
        .get(&url)
        .header("host", &host)
        .header("x-amz-content-sha256", &content_sha256)
        .header("x-amz-date", &datetime)
        .header("authorization", &authorization)
        .send()
        .await
        .context("ошибка скачивания из S3")?;

    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("S3 GET ошибка {status}: {body}");
    }
    Ok(res.bytes().await?.to_vec())
}

fn endpoint_host(endpoint: &str) -> String {
    endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}
