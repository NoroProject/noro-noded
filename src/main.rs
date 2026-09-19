//! `noro-noded` — демон ноды.
//!
//! Держит игровые серверы в контейнерах и ходит к мастеру исходящим
//! WebSocket. Контейнеры переживают его перезапуск намеренно: падение
//! управляющего процесса — не повод ронять чужую игру, поэтому при старте
//! демон пересчитывает их заново и докладывает, что нашёл.

mod backup;
mod config;
mod docker;
mod fs;
mod http;
mod link;
mod metrics;
mod server;
mod sftp;

use anyhow::Result;
use clap::Parser;
use schema::noded::NodeEvent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::docker::Engine;
use crate::link::master::MasterClient;
use crate::server::registry::Registry;

#[derive(Parser)]
#[command(name = "noro-noded", about = "Демон ноды Noro")]
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
    let cfg = Arc::new(Config::load(&cli.config)?);
    std::fs::create_dir_all(cfg.data_dir.join("servers"))?;

    let engine = Engine::connect(&cfg.docker.socket)?;
    let version = engine.version().await?;
    tracing::info!(docker = %version, data_dir = %cfg.data_dir.display(), "демон запускается");

    let master = MasterClient::new(&cfg.master_url, &cfg.token)?;
    let (events_tx, events_rx) = mpsc::channel::<NodeEvent>(1024);

    let daemon = Daemon {
        cfg: cfg.clone(),
        engine: engine.clone(),
        master,
        registry: Registry::default(),
        events: events_tx.clone(),
        sftp_fingerprint: Arc::new(parking_lot::Mutex::new(None)),
        http_tickets: http::Tickets::default(),
        quota: fs::quota::Quota::default(),
    };

    // Переподключиться к тому, что уже работает, до первого кадра мастеру:
    // иначе консоль живого сервера молчала бы до его следующего рестарта.
    reattach_running(&daemon).await;
    start_sftp(&daemon).await;
    // Прямая передача — отдельной задачей: её слушатель живёт весь срок работы
    // демона, а канал с мастером ниже занимает основной поток.
    tokio::spawn(http::serve(daemon.clone()));
    spawn_node_stats(daemon.clone(), events_tx.clone());
    spawn_server_stats(daemon.clone(), events_tx);

    link::session::run(daemon, events_rx).await
}

/// Замеры по каждому работающему серверу.
///
/// Без этой задачи `NodeEvent::Stats` не отправлялся вовсе: мастер его ждал, а
/// графики в панели рисовали пустоту. Раз в двадцать секунд — чаще незачем,
/// докер отдаёт счётчики с секундным шагом, а обход каталога под диск не
/// бесплатен.
fn spawn_server_stats(daemon: Daemon, events: mpsc::Sender<NodeEvent>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
        loop {
            ticker.tick().await;
            for server in daemon.registry.ids() {
                let up = daemon
                    .registry
                    .get(server)
                    .map(|h| h.state().power.is_up())
                    .unwrap_or(false);
                if !up {
                    continue;
                }
                let root = daemon.cfg.server_dir(server);
                match daemon
                    .engine
                    .stats_with_disk(server, &daemon.quota, &root)
                    .await
                {
                    Ok(stats) => {
                        let _ = events.send(NodeEvent::Stats { server, stats }).await;
                    }
                    // Контейнер могли снять между перечислением и замером —
                    // это не повод шуметь в лог на каждом тике.
                    Err(e) => tracing::debug!(%server, error = %e, "замер не удался"),
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
    };
    match sftp::server::start(server, &daemon.cfg.sftp.bind).await {
        Ok(fingerprint) => {
            tracing::info!(bind = %daemon.cfg.sftp.bind, %fingerprint, "sftp поднят");
            daemon.sftp_fingerprint.lock().replace(fingerprint);
        }
        Err(e) => tracing::error!(error = %format!("{e:#}"), "sftp не поднялся"),
    }
}

/// Метрики машины раз в полминуты: чаще панели не нужно, а sysinfo не бесплатен.
fn spawn_node_stats(daemon: Daemon, events: mpsc::Sender<NodeEvent>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let running = daemon
                .registry
                .ids()
                .iter()
                .filter(|id| {
                    daemon
                        .registry
                        .get(**id)
                        .map(|h| h.state().power.is_up())
                        .unwrap_or(false)
                })
                .count() as u32;
            let _ = events
                .send(NodeEvent::NodeStats {
                    stats: metrics::sample(running),
                })
                .await;
        }
    });
}
