// Файл превышает 150 строк: сборка демона и его фоновые задачи (уборка, переподключение, SFTP) — одна точка старта.
//! `noro-noded` — демон ноды.
//!
//! Держит игровые серверы в контейнерах и ходит к мастеру исходящим
//! WebSocket. Контейнеры переживают его перезапуск намеренно: падение
//! управляющего процесса — не повод ронять чужую игру, поэтому при старте
//! демон пересчитывает их заново и докладывает, что нашёл.

mod backup;
mod config;
mod diag;
mod docker;
mod fs;
mod http;
mod link;
mod metrics;
mod net;
mod sampling;
mod server;
mod sftp;

use anyhow::Result;
use clap::Parser;
use schema::noded::NodeEvent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::Config;
use crate::docker::Engine;
use crate::link::master::MasterClient;
use crate::server::registry::Registry;

#[derive(Parser)]
// `version` — не украшение: скрипт обновления сверяет по нему установленное
// с тем, что раскатано на мастере, и без него ему пришлось бы качать всегда.
#[command(name = "noro-noded", version, about = "Демон ноды Noro")]
struct Cli {
    /// Путь к конфигу.
    #[arg(
        short,
        long,
        default_value = "/etc/noro/noded.toml",
        env = "NORO_NODED_CONFIG"
    )]
    config: PathBuf,
}

/// Всё, что нужно любой операции. Клонируется дёшево — внутри только ручки.
#[derive(Clone)]
pub struct Daemon {
    pub cfg: Arc<Config>,
    pub engine: Engine,
    pub master: MasterClient,
    pub registry: Registry,
    /// Исходящие события: консоль, состояния, метрики.
    pub events: mpsc::Sender<NodeEvent>,
    /// Отпечаток хост-ключа SFTP — появляется, когда сервер поднялся.
    pub sftp_fingerprint: Arc<parking_lot::Mutex<Option<String>>>,
    /// Пропуска прямой передачи: выписывает мастер, предъявляет браузер.
    pub http_tickets: http::Tickets,
    /// Сколько места занимают серверы и сколько им можно.
    pub quota: fs::quota::Quota,
    /// Порты, которые демон держит за спящими серверами.
    pub placeholders: Arc<dashmap::DashMap<Uuid, server::waker::Placeholder>>,
    /// Ограничитель побудок: стучать в порт спящего сервера может кто угодно.
    pub wake_gate: server::wake_gate::WakeGate,
    /// Какие серверы заняты тяжёлой операцией.
    pub busy: server::busy::Busy,
    /// Связь с мастером и прочее состояние самого демона.
    pub health: Arc<diag::Health>,
    /// Сети серверов и правила фаервола поверх них.
    pub isolation: Arc<net::isolation::Isolation>,
}

impl Daemon {
    pub fn sftp_port(&self) -> u16 {
        self.cfg
            .sftp
            .bind
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(2022)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,noded=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let mut cfg = Config::load(&cli.config)?;
    std::fs::create_dir_all(cfg.data_dir.join("servers"))?;
    // Канонический путь, а не как записали в конфиге: запись в каталог сервера
    // сверяет настоящий путь открытого каталога с ожидаемым буквально, и
    // ссылка где-то в пути к каталогу данных (`/srv -> /mnt/disk`) иначе
    // выглядела бы подменой.
    cfg.data_dir = cfg.data_dir.canonicalize()?;
    let cfg = Arc::new(cfg);

    let engine = Engine::connect(&cfg.docker.socket)?;
    let version = engine.version().await?;
    tracing::info!(docker = %version, data_dir = %cfg.data_dir.display(), "демон запускается");

    let isolation = Arc::new(net::isolation::Isolation::new(
        &cfg,
        Arc::new(net::firewall::SystemIptables),
    )?);
    let (projects, quota_mode) = fs::project::detect(&cfg.data_dir, cfg.project_quota);
    let master = MasterClient::new(&cfg.master_url, &cfg.token)?;
    let (events_tx, events_rx) = mpsc::channel::<NodeEvent>(1024);

    let daemon = Daemon {
        cfg: cfg.clone(),
        engine: engine.clone(),
        master,
        registry: Registry::new(server::crash::CrashStore::new(cfg.clone())),
        events: events_tx.clone(),
        sftp_fingerprint: Arc::new(parking_lot::Mutex::new(None)),
        http_tickets: http::Tickets::default(),
        quota: match cfg.disk_reserve_mb {
            0 => fs::quota::Quota::default(),
            mb => {
                fs::quota::Quota::with_reserve(fs::reserve::Reserve::new(cfg.data_dir.clone(), mb))
            }
        }
        .with_projects(projects, quota_mode),
        placeholders: Arc::new(dashmap::DashMap::new()),
        wake_gate: server::wake_gate::WakeGate::default(),
        busy: server::busy::Busy::default(),
        health: Arc::new(diag::Health::default()),
        isolation,
    };

    // Правила — до первого сервера и до мастера: после перезагрузки машины
    // фаервол пуст, а контейнеры поднимутся по первой же команде.
    daemon.isolation.enforce(&daemon.engine).await;
    spawn_firewall_repair(&daemon);

    // Переподключиться к тому, что уже работает, до первого кадра мастеру:
    // иначе консоль живого сервера молчала бы до его следующего рестарта.
    reattach_running(&daemon).await;
    start_sftp(&daemon).await;
    // Прямая передача — отдельной задачей: её слушатель живёт весь срок работы
    // демона, а канал с мастером ниже занимает основной поток.
    tokio::spawn(http::serve(daemon.clone()));
    sampling::spawn_node_stats(daemon.clone(), events_tx.clone());
    spawn_sweep(&daemon);
    sampling::spawn_server_stats(daemon.clone(), events_tx);

    link::session::run(daemon, events_rx).await
}

/// Поставить правила изоляции заново раз в минуту: их стирают перезагрузка
/// фаервола и руки, а ждать перезапуска демона нельзя.
fn spawn_firewall_repair(daemon: &Daemon) {
    if !daemon.isolation.enabled() {
        return;
    }
    let daemon = daemon.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            daemon.isolation.enforce(&daemon.engine).await;
        }
    });
}

/// Убрать временные файлы оборванных до рестарта операций — в фоне, по
/// одному серверу: это обход каталогов, и старт демона он ждать не должен.
fn spawn_sweep(daemon: &Daemon) {
    let cfg = daemon.cfg.clone();
    let quota = daemon.quota.clone();
    tokio::task::spawn_blocking(move || {
        let Ok(dirs) = std::fs::read_dir(cfg.data_dir.join("servers")) else {
            return;
        };
        for dir in dirs.flatten() {
            if !dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let removed = fs::sweep::sweep(&dir.path(), fs::sweep::MIN_AGE);
            if removed > 0 {
                tracing::info!(server = %dir.file_name().to_string_lossy(), removed, "убраны недописанные временные файлы");
            }
            let Some(server) = dir
                .file_name()
                .to_str()
                .and_then(|n| Uuid::parse_str(n).ok())
            else {
                continue;
            };
            quota.adopt(server, &dir.path());
            // Архивы прежней раскладки переносятся сразу, а не при первом
            // бэкапе: пока они в папке сервера, до них дотягивается его процесс.
            let backups = cfg.backup_dir(server);
            match backup::store::prepare(&backups, &dir.path()) {
                Ok(()) => {
                    fs::sweep::sweep(&backups, fs::sweep::MIN_AGE);
                }
                Err(e) => {
                    tracing::warn!(%server, error = %format!("{e:#}"), "каталог бэкапов не подготовлен")
                }
            }
        }
    });
}

/// Найти свои контейнеры и снова сесть им на консоль.
async fn reattach_running(daemon: &Daemon) {
    let found = match daemon.engine.list_managed().await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "не перечислить контейнеры");
            return;
        }
    };

    for item in found {
        let Some(server) = item.server else {
            // Контейнер с нашей меткой, но без идентификатора сервера. Удалять
            // его самим нельзя — это делается человеком в панели.
            tracing::warn!(container = %item.name, "контейнер без метки сервера");
            continue;
        };
        if !item.state.is_up() {
            continue;
        }
        match server::supervisor::attach(
            daemon.engine.clone(),
            daemon.registry.clone(),
            daemon.events.clone(),
            server,
        )
        .await
        {
            Ok(()) => tracing::info!(%server, "переподключён к работающему серверу"),
            Err(e) => tracing::warn!(%server, error = %e, "не переподключиться к консоли"),
        }
    }
}

/// Поднять SFTP, если он включён в конфиге.
///
/// Отпечаток хост-ключа уезжает мастеру в `hello` — человек сверяет его в
/// панели вместо того, чтобы отвечать `yes` на вопрос о незнакомом ключе.
async fn start_sftp(daemon: &Daemon) {
    if !daemon.cfg.sftp.enabled {
        return;
    }
    let server = sftp::server::SftpServer {
        master: daemon.master.clone(),
        data_dir: daemon.cfg.data_dir.clone(),
        events: daemon.events.clone(),
        quota: daemon.quota.clone(),
    };
    match sftp::server::start(server, &daemon.cfg.sftp.bind).await {
        Ok(fingerprint) => {
            tracing::info!(bind = %daemon.cfg.sftp.bind, %fingerprint, "sftp поднят");
            daemon.sftp_fingerprint.lock().replace(fingerprint);
        }
        Err(e) => tracing::error!(error = %format!("{e:#}"), "sftp не поднялся"),
    }
}
