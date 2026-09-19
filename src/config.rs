//! Конфиг демона: `/etc/noro/noded.toml` плюс переменные окружения.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Корень мастера без хвостового слэша.
    pub master_url: String,
    /// Секрет ноды (`noronode_…`). Отсюда мастер и узнаёт, какая это нода.
    pub token: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Тот же каталог, но каким его видит **хост**.
    ///
    /// Расходится с `data_dir`, когда демон сам работает в контейнере: bind'ы
    /// докер резолвит на хосте, и путь изнутри контейнера даёт молча пустой
    /// маунт вместо каталога сервера. Пусто — значит пути совпадают.
    #[serde(default)]
    pub host_data_dir: Option<PathBuf>,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub sftp: SftpConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DockerConfig {
    #[serde(default = "default_socket")]
    pub socket: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_bind")]
    pub bind: String,
    /// Адрес, по которому до демона достаёт браузер. Пусто — крупные файлы
    /// пойдут через мастер, и это рабочий режим для ноды за NAT.
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SftpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sftp_bind")]
    pub bind: String,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/noro-noded")
}

fn default_socket() -> String {
    "/var/run/docker.sock".into()
}

fn default_http_bind() -> String {
    "0.0.0.0:8444".into()
}

fn default_sftp_bind() -> String {
    "0.0.0.0:2022".into()
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            bind: default_http_bind(),
            public_url: None,
        }
    }
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_sftp_bind(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("не прочитать конфиг {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).context("конфиг не разобран")?;

        // Секрет удобнее подсунуть переменной: в контейнере файл с ним пришлось
        // бы монтировать отдельно ради одной строки.
        if let Ok(token) = std::env::var("NORO_NODE_TOKEN") {
            if !token.is_empty() {
                cfg.token = token;
            }
        }
        if let Ok(url) = std::env::var("NORO_MASTER_URL") {
            if !url.is_empty() {
                cfg.master_url = url;
            }
        }

        cfg.master_url = cfg.master_url.trim_end_matches('/').to_string();
        if cfg.master_url.is_empty() {
            anyhow::bail!("master_url пуст");
        }
        if cfg.token.is_empty() {
            anyhow::bail!("token пуст");
        }
        Ok(cfg)
    }

    /// Каталог сервера так, как его видит сам демон.
    pub fn server_dir(&self, id: uuid::Uuid) -> PathBuf {
        self.data_dir.join("servers").join(id.to_string())
    }

    /// Тот же каталог глазами докера. Именно это уходит в bind контейнера.
    pub fn host_server_dir(&self, id: uuid::Uuid) -> PathBuf {
        self.host_data_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.clone())
            .join("servers")
            .join(id.to_string())
    }

    pub fn ws_url(&self) -> String {
        let base = self
            .master_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/api/node/ws")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(master: &str) -> Config {
        Config {
            master_url: master.into(),
            token: "noronode_x".into(),
            data_dir: PathBuf::from("/var/lib/noro-noded"),
            host_data_dir: None,
            docker: DockerConfig::default(),
            http: HttpConfig::default(),
            sftp: SftpConfig::default(),
        }
    }

    #[test]
    fn ws_url_follows_the_master_scheme() {
        assert_eq!(
            cfg("https://api.example.com").ws_url(),
            "wss://api.example.com/api/node/ws"
        );
        assert_eq!(
            cfg("http://localhost:8080").ws_url(),
            "ws://localhost:8080/api/node/ws"
        );
    }

    /// Без этого bind уезжает в путь, которого на хосте нет, и сервер стартует
    /// с пустым каталогом вместо своего мира.
    #[test]
    fn host_path_differs_when_the_daemon_is_containerised() {
        let mut c = cfg("https://api.example.com");
        c.data_dir = PathBuf::from("/data");
        c.host_data_dir = Some(PathBuf::from("/opt/noro/noded"));
        let id = uuid::Uuid::nil();
        assert_eq!(
            c.server_dir(id),
            PathBuf::from("/data/servers").join(id.to_string())
        );
        assert_eq!(
            c.host_server_dir(id),
            PathBuf::from("/opt/noro/noded/servers").join(id.to_string())
        );
    }

    #[test]
    fn host_path_defaults_to_the_local_one() {
        let c = cfg("https://api.example.com");
        let id = uuid::Uuid::nil();
        assert_eq!(c.server_dir(id), c.host_server_dir(id));
    }
}
