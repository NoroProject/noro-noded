// Файл превышает 150 строк: подпись SigV4 и все запросы к S3, которыми пользуется нода, — один клиент.
//! Клиент S3-совместимого хранилища: подпись SigV4 и запросы, нужные бэкапам.
//!
//! Своя подпись вместо SDK: ноде нужны пять запросов, а SDK тащит за собой
//! половину AWS.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize)]
pub struct S3Config {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub path_prefix: String,
}

fn default_endpoint() -> String {
    "https://s3.amazonaws.com".into()
}

fn default_region() -> String {
    "us-east-1".into()
}

impl S3Config {
    pub fn full_key(&self, filename: &str) -> String {
        let prefix = self.path_prefix.trim().trim_matches('/');
        if prefix.is_empty() {
            filename.trim_start_matches('/').to_string()
        } else {
            format!("{prefix}/{}", filename.trim_start_matches('/'))
        }
    }

    fn host(&self) -> String {
        self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string()
    }

    fn object_path(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, key.trim_start_matches('/'))
    }
}

/// sha256 пустого тела.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Запрос к объекту: метод, ключ, параметры строки запроса и подписанные
/// заголовки сверх обязательных.
pub struct Request<'a> {
    pub method: reqwest::Method,
    pub key: &'a str,
    pub query: Vec<(&'a str, String)>,
    pub headers: Vec<(&'a str, String)>,
    pub payload_sha256: String,
}

impl Request<'_> {
    /// Собрать подписанный запрос. Тело добавляет вызывающий.
    pub fn build(&self, client: &Client, cfg: &S3Config) -> reqwest::RequestBuilder {
        let path = cfg.object_path(self.key);
        let query = canonical_query(&self.query);
        let mut headers = vec![
            ("host".to_string(), cfg.host()),
            (
                "x-amz-content-sha256".to_string(),
                self.payload_sha256.clone(),
            ),
            (
                "x-amz-date".to_string(),
                Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
            ),
        ];
        headers.extend(
            self.headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone())),
        );
        let authorization = authorization(
            cfg,
            self.method.as_str(),
            &path,
            &query,
            &mut headers,
            &self.payload_sha256,
        );

        let mut url = format!("{}{path}", cfg.endpoint.trim_end_matches('/'));
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        let mut req = client.request(self.method.clone(), url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.header("authorization", authorization)
    }
}

/// Отправить и отказать на любом ответе, кроме успешного.
pub async fn send(req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
    let res = req
        .send()
        .await
        .with_context(|| format!("S3 {what}: запрос не ушёл"))?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("S3 {what} ошибка {status}: {body}");
    }
    Ok(res)
}

/// Заголовок `Authorization` по SigV4. `headers` сортируются на месте: в
/// подписи и в запросе они должны идти одинаково. Время подписи — из
/// заголовка `x-amz-date`: подпись обязана быть ровно под него.
fn authorization(
    cfg: &S3Config,
    method: &str,
    path: &str,
    query: &str,
    headers: &mut [(String, String)],
    payload_sha256: &str,
) -> String {
    let datetime = headers
        .iter()
        .find(|(k, _)| k == "x-amz-date")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let date = datetime.get(..8).unwrap_or_default().to_string();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", cfg.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let key = signing_key(&cfg.secret_key, &date, &cfg.region);
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
        cfg.access_key
    )
}

/// Строка запроса в каноническом виде: по ключам, каждая часть закодирована.
fn canonical_query(query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn signing_key(secret_key: &str, date: &str, region: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пример из документации AWS («GET Object», SigV4): подпись, совпавшая с
    /// ним, совпадёт и у настоящего хранилища.
    #[test]
    fn the_signature_matches_the_aws_example() {
        let cfg = S3Config {
            endpoint: "https://examplebucket.s3.amazonaws.com".into(),
            region: "us-east-1".into(),
            bucket: "examplebucket".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            path_prefix: String::new(),
        };
        let mut headers = vec![
            (
                "host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization(&cfg, "GET", "/test.txt", "", &mut headers, EMPTY_SHA256);
        assert!(
            auth.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{auth}"
        );
    }

    #[test]
    fn the_upload_id_is_encoded_in_the_query() {
        let q = canonical_query(&[("uploadId", "a/b+c=".into()), ("partNumber", "2".into())]);
        assert_eq!(q, "partNumber=2&uploadId=a%2Fb%2Bc%3D");
    }
}
