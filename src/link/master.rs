//! HTTP к мастеру: скачивание файлов и вызовы, которым сокет не нужен.
//!
//! Нода никогда не шлёт мастеру запросы по управляющему сокету — только ответы
//! и события. Всё, что ей самой нужно спросить, идёт обычным HTTPS со своим
//! токеном: так состояние канала остаётся односторонним и в нём нечему
//! зациклиться.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha1::Digest;
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Clone)]
pub struct MasterClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl MasterClient {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("noro-noded/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    /// Скачать файл во временный и переименовать, сверив хеш.
    ///
    /// Именно в таком порядке: файл появляется на своём месте только целым.
    /// Оборванная закачка иначе оставляет полумод, который сервер честно
    /// попытается загрузить.
    pub async fn download(
        &self,
        url: &str,
        dest: &Path,
        sha1: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<u64> {
        let parent = dest
            .parent()
            .context("у пути загрузки нет каталога")?
            .to_path_buf();
        let name = dest
            .file_name()
            .context("у пути загрузки нет имени файла")?
            .to_owned();
        tokio::fs::create_dir_all(&parent).await?;

        // Токен уходит только на свой же мастер: артефакты вроде ядра лежат на
        // чужих адресах, и Authorization туда слать незачем.
        let mut req = self.http.get(url);
        if url.starts_with(&self.base) {
            req = req.bearer_auth(&self.token);
        }

        let response = req
            .send()
            .await
            .with_context(|| format!("не скачать {url}"))?;
        if !response.status().is_success() {
            bail!("{url} ответил {}", response.status());
        }

        // Каталог — чаще всего папка сервера, а её меняет процесс сервера: на
        // месте временного файла могла лежать ссылка на файл хоста, а сам
        // каталог — оказаться ссылкой к концу закачки. Поэтому каталог
        // закрепляется до неё, а временный файл создаётся в нём эксклюзивно.
        let dir = crate::fs::nofollow::Dir::open(&parent)?;
        let tmp = {
            let mut tmp = std::ffi::OsString::from(".");
            tmp.push(&name);
            tmp.push(format!(".noro-part-{}", uuid::Uuid::new_v4().simple()));
            dir.entry(tmp)
        };
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;

        // Имя временного файла случайное, и следующая попытка его не найдёт:
        // оборванная закачка обязана убрать его сама, иначе каждая оставляла бы
        // в папке сервера по недокачанному моду.
        let fetched = async {
            let mut sha1_hasher = sha1::Sha1::new();
            let mut sha256_hasher = sha2::Sha256::new();
            let mut size = 0u64;

            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                size += chunk.len() as u64;
                sha1_hasher.update(&chunk);
                sha256_hasher.update(&chunk);
                file.write_all(&chunk).await?;
            }
            file.flush().await?;

            if let Some(expected) = sha1 {
                let got = hex::encode(sha1_hasher.finalize());
                if !got.eq_ignore_ascii_case(expected) {
                    bail!("sha1 не сошёлся: ждали {expected}, получили {got}");
                }
            }
            if let Some(expected) = sha256 {
                let got = hex::encode(sha256_hasher.finalize());
                if !got.eq_ignore_ascii_case(expected) {
                    bail!("sha256 не сошёлся: ждали {expected}, получили {got}");
                }
            }
            Ok(size)
        }
        .await;
        drop(file);
        let size = match fetched {
            Ok(size) => size,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e).with_context(|| format!("не скачать {url}"));
            }
        };

        if let Err(e) = tokio::fs::rename(&tmp, dir.entry(&name)).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        Ok(size)
    }
}
