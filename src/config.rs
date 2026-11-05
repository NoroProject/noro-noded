// Файл превышает 150 строк: разделы конфига с их умолчаниями и проверки путей в тестах рядом.
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
    /// Сколько мегабайт на диске с каталогом данных не отдавать никому: при
    /// меньшем остатке заливки, распаковки и скачивания получают отказ, чтобы
    /// работающим серверам было куда сохраняться. Ноль — запаса нет.
    #[serde(default = "default_disk_reserve_mb")]
    pub disk_reserve_mb: u64,
    /// Держать квоту диска средствами ФС (project quota на xfs/ext4), если
    /// она включена на томе с каталогом данных. `false` — всегда обход.
    #[serde(default = "yes")]
    pub project_quota: bool,
    #[serde(default)]
    pub network: NetworkConfig,
}

/// Изоляция сети серверов — см. `net/rules.rs`.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// Своя сеть у каждого нового или пересобранного контейнера и правила
    /// фаервола поверх. Старые контейнеры получают её при пересборке.
    #[serde(default = "yes")]
    pub isolation: bool,
    /// Откуда нарезаются подсети /28 под сети серверов. Не должен пересекаться
    /// с сетями машины: маршрут в такую подсеть уйдёт в мост, а не наружу.
    #[serde(default = "default_subnet_pool")]
    pub subnet_pool: String,
    /// Что серверам всё же можно: `10.0.0.5`, `10.0.0.0/24`, `10.0.0.5:3306`.
    #[serde(default)]
    pub allow: Vec<String>,
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

fn default_disk_reserve_mb() -> u64 {
    2048
}

fn yes() -> bool {
    true
}

fn default_subnet_pool() -> String {
    "10.209.0.0/16".into()
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

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            isolation: true,
            subnet_pool: default_subnet_pool(),
            allow: Vec::new(),
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

    /// Архивы бэкапов сервера.
    ///
    /// Рядом с каталогом сервера, а не в нём: тот примонтирован в контейнер, и
    /// процесс сервера мог удалить точки восстановления или подменить архив
    /// своим — а восстанавливают из него, когда мир уже испорчен.
    pub fn backup_dir(&self, id: uuid::Uuid) -> PathBuf {
        self.data_dir.join("backups").join(id.to_string())
    }

    /// Отпечатки падений сервера — по той же причине вне его каталога.
    pub fn crash_dir(&self, id: uuid::Uuid) -> PathBuf {
        self.data_dir.join("crashes").join(id.to_string())
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
            disk_reserve_mb: default_disk_reserve_mb(),
            project_quota: true,
            network: NetworkConfig::default(),
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

    /// Конфиг ноды, заведённой до изоляции, разбирается и получает её
    /// включённой: так её получают все новые контейнеры.
    #[test]
    fn an_old_config_gets_isolation_and_project_quota_on() {
        let c: Config = toml::from_str("master_url = \"https://m\"\ntoken = \"t\"\n").unwrap();
        assert!(c.network.isolation);
        assert!(c.project_quota);
        assert_eq!(c.network.subnet_pool, "10.209.0.0/16");
        assert!(c.network.allow.is_empty());

        let c: Config = toml::from_str(
            "master_url = \"https://m\"\ntoken = \"t\"\nproject_quota = false\n\
             [network]\nisolation = false\nallow = [\"10.0.0.5:3306\"]\n",
        )
        .unwrap();
        assert!(!c.network.isolation && !c.project_quota);
        assert_eq!(c.network.allow, vec!["10.0.0.5:3306"]);
    }

    #[test]
    fn host_path_defaults_to_the_local_one() {
        let c = cfg("https://api.example.com");
        let id = uuid::Uuid::nil();
        assert_eq!(c.server_dir(id), c.host_server_dir(id));
    }
}
