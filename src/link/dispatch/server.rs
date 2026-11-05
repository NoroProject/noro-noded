// Файл превышает 150 строк: пересборка контейнера, установка и питание — операции над сервером целиком, и делят одни помощники.
//! Операции над сервером целиком: контейнер, установка, пак, питание, консоль.

use schema::build::FileEntry;
use schema::noded::{
    InstallSpec, NodeEvent, OpError, PackSource, PowerAction, PowerState, ServerSpec, SyncPolicy,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{blocking, docker_error, OpResult};
use crate::docker::create::CreateArgs;
use crate::fs;
use crate::server::{install, layout::Layout, supervisor};
use crate::Daemon;

pub async fn apply_spec(daemon: &Daemon, server: Uuid, spec: Box<ServerSpec>) -> OpResult {
    let layout = daemon.layout(server);
    layout
        .prepare()
        .map_err(|e| OpError::new("docker_failed", e.to_string()))?;

    if let Some(port) = spec
        .ports
        .iter()
        .find(|p| p.primary)
        .map(|p| p.port)
        .or_else(|| spec.ports.first().map(|p| p.port))
    {
        let _ = layout.write_primary_port(port);
        let _ = crate::server::props::ensure_port(&layout.root.join("server.properties"), port);
    }

    // Чем запускать, решает диск, а не карточка сервера: имя мог
    // сменить установщик. Найденное уезжает в ответе — мастеру надо
    // запомнить его, иначе следующая пересборка начнёт с того же.
    let mut spec = *spec;
    // Сервер из собственного образа (лимбо) запускается точкой входа образа:
    // jar, найденный в его каталоге, превратил бы его в Java-сервер.
    if !(spec.jar.is_empty() && !spec.image.starts_with("eclipse-temurin:")) {
        spec.jar = install::resolve_entry_point(&layout.root, &spec.jar);
    }
    let mut spec = Box::new(spec);

    // Лимит кладётся на диск рядом с сервером: демон перезапускается, а
    // квота должна знать потолок и после этого — в памяти он бы пропал.
    let limits = fs::quota::Limits {
        disk_mb: spec.disk_mb,
    };
    if let Err(e) = fs::quota::write_limits(&layout.root, &limits) {
        tracing::warn!(%server, error = %e, "лимит диска не записан");
    }
    daemon.quota.measure(server, &layout.root).await;

    // Строка запуска ссылается на authlib-injector, а пересоздание
    // идёт мимо установки: без этой докачки контейнер, которому
    // аккаунты лаунчера включили после установки, не стартовал бы
    // вовсе.
    install::ensure_authlib(&daemon.master, &layout, &spec)
        .await
        .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))?;

    // Пересоздание вместо правки: докер не умеет менять лимиты живого
    // контейнера, а притворяться, что умеет, — значит разойтись с тем,
    // что показывает панель.
    //
    // Живой сервер сначала гасится по-человечески. `remove` идёт с
    // `force`, то есть SIGKILL: правка лимитов не должна стоить
    // игрокам несохранённого мира. Что работало — поднимаем обратно.
    let running = daemon.engine.was_running(server).await;
    let handle = daemon.registry.get_or_create(server);
    if daemon.engine.exists(server).await {
        // Служебные переменные (NORO_AGENT_SECRET и др.) переносим из прежнего
        // контейнера, если мастер не прислал новые явно.
        if let Ok(inspect) = daemon
            .engine
            .docker
            .inspect_container(&crate::docker::engine::Engine::container_name(server), None)
            .await
        {
            if let Some(existing_env) = inspect.config.and_then(|c| c.env) {
                for entry in existing_env {
                    if let Some((k, v)) = entry.split_once('=') {
                        if k.starts_with("NORO_") && !spec.env.contains_key(k) {
                            spec.env.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            }
        }

        if running {
            handle.stopping_on_purpose();
            daemon.engine.stop(server).await.ok();
        }
        daemon.engine.remove(server).await.ok();
    }
    daemon
        .engine
        .create_container(CreateArgs {
            server,
            spec: &spec,
            host_dir: &daemon.cfg.host_server_dir(server),
        })
        .await
        .map_err(|e| OpError::new("docker_failed", e.to_string()))?;

    if running {
        handle.starting_on_purpose();
        daemon
            .engine
            .start(server)
            .await
            .map_err(|e| OpError::new("docker_failed", e.to_string()))?;
        attach(daemon, server).await?;
    }
    Ok(json!({ "jar": spec.jar }))
}

pub async fn delete(daemon: &Daemon, server: Uuid, keep_files: bool) -> OpResult {
    // Держатель порта спящего сервера иначе переживал сам сервер и
    // занимал порт до перезапуска демона — новому серверу на этом
    // порту было не стартовать.
    daemon.placeholders.remove(&server);
    if daemon.engine.exists(server).await {
        daemon.engine.remove(server).await.ok();
    }
    daemon.registry.forget(server);
    daemon.quota.forget(server);
    if !keep_files {
        let dir = daemon.cfg.server_dir(server);
        let _ = std::fs::remove_file(fs::quota::limits_path(&dir));
        // Архивы и отпечатки падений живут вне каталога сервера, но без него
        // им незачем жить: прежде они уходили вместе с ним.
        let leftovers = [
            dir,
            daemon.cfg.backup_dir(server),
            daemon.cfg.crash_dir(server),
        ];
        blocking(move || {
            for dir in leftovers {
                if std::fs::symlink_metadata(&dir).is_ok() {
                    fs::nofollow::remove(&dir)?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| OpError::new("docker_failed", e.to_string()))?;
    }
    Ok(Value::Null)
}

pub async fn install(daemon: &Daemon, server: Uuid, install: Box<InstallSpec>) -> OpResult {
    let layout = daemon.layout(server);
    let (log_tx, log_rx) = mpsc::channel::<String>(64);
    spawn_install_log(daemon.events.clone(), log_rx, server);

    let ctx = install::InstallCtx {
        engine: &daemon.engine,
        master: &daemon.master,
        layout: &layout,
        host_dir: &daemon.cfg.host_server_dir(server),
        server,
        log: log_tx,
    };
    install::install(ctx, &install)
        .await
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
        .map_err(|e| OpError::new("panel_install_failed", format!("{e:#}")))
}

pub async fn pack_apply(daemon: &Daemon, server: Uuid, pack: PackSource) -> OpResult {
    // Пак меняет jar'ы и моды; под живой JVM это верное падение. Мастер
    // гасит сервер сам — здесь страховка от гонки с кнопкой «Старт».
    if let Ok((state, _, _)) = daemon.engine.state(server).await {
        if matches!(
            state,
            PowerState::Running | PowerState::Starting | PowerState::Stopping
        ) {
            return Err(OpError::new(
                "panel_install_failed",
                "сервер запущен: пак кладётся только на остановленный",
            ));
        }
    }
    let layout = daemon.layout(server);
    let laid = crate::server::pack::lay_down(&daemon.master, &layout, &pack)
        .await
        .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))?;
    crate::server::layout::chown_recursive(&layout.root)
        .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;
    Ok(json!({ "bytes": laid.bytes, "untracked": laid.untracked }))
}

pub async fn command(daemon: &Daemon, server: Uuid, line: String) -> OpResult {
    let handle = daemon
        .registry
        .get(server)
        .ok_or_else(|| OpError::new("node_offline", "сервер не запущен"))?;
    let mut writer = handle.writer.lock().await;
    let writer = writer
        .as_mut()
        .ok_or_else(|| OpError::new("node_offline", "консоль недоступна"))?;
    writer
        .send(&line)
        .await
        .map(|_| Value::Null)
        .map_err(|e| OpError::new("docker_failed", e.to_string()))
}

pub async fn clone_files(
    daemon: &Daemon,
    server: Uuid,
    from: Uuid,
    include_worlds: bool,
) -> OpResult {
    let to = daemon.cfg.server_dir(server);
    let source = daemon.cfg.server_dir(from);
    // Blocking copy on the blocking pool: a world is gigabytes, and
    // doing that on the async runtime stalls every other server's
    // console on this node.
    tokio::task::spawn_blocking(move || crate::server::clone::run(&source, &to, include_worlds))
        .await
        .map_err(|e| OpError::new("upstream_failed", format!("{e}")))?
        .map(|r| serde_json::json!({ "files": r.files, "bytes": r.bytes }))
        .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))
}

pub async fn build_sync(
    daemon: &Daemon,
    server: Uuid,
    files: Vec<FileEntry>,
    policy: SyncPolicy,
) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    crate::server::sync::run(&daemon.master, &root, &files, &policy)
        .await
        .map(|report| serde_json::to_value(report).unwrap_or(Value::Null))
        .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))
}

pub async fn power(daemon: &Daemon, server: Uuid, action: PowerAction) -> OpResult {
    let engine = &daemon.engine;
    // Намерение человека важнее автоподъёма: без этой пометки присмотр поднял
    // бы сервер, который только что погасили — в том числе приостановленный,
    // который мастер гасит этой же командой.
    let handle = daemon.registry.get_or_create(server);
    match action {
        PowerAction::Start => {
            handle.starting_on_purpose();
            daemon.ensure_server_port(server).await;
        }
        PowerAction::Restart => {
            // Остановка внутри перезапуска — тоже намеренная. Иначе выход с
            // ненулевым кодом (Forge часто выходит так и на `stop`) присмотр
            // прошлого запуска принимал за падение и сам брался поднимать.
            handle.stopping_on_purpose();
            handle.next_generation();
            daemon.ensure_server_port(server).await;
        }
        PowerAction::Stop | PowerAction::Kill => handle.stopping_on_purpose(),
    }

    if matches!(action, PowerAction::Start | PowerAction::Restart) {
        // Контейнеру нужен его порт. Мастер снимает держатель сам, но если тот
        // по какой-то причине остался, запуск упирался в занятый порт.
        daemon.placeholders.remove(&server);
    }

    let result = match action {
        PowerAction::Start => {
            engine.start(server).await.map_err(docker_error)?;
            return attach(daemon, server).await;
        }
        PowerAction::Stop => engine.stop(server).await,
        PowerAction::Kill => engine.kill(server).await,
        PowerAction::Restart => {
            engine.stop(server).await.ok();
            handle.starting_on_purpose();
            engine.start(server).await.map_err(docker_error)?;
            return attach(daemon, server).await;
        }
    };
    result.map(|_| Value::Null).map_err(docker_error)
}

pub async fn attach(daemon: &Daemon, server: Uuid) -> OpResult {
    supervisor::attach(
        daemon.engine.clone(),
        daemon.registry.clone(),
        daemon.events.clone(),
        server,
    )
    .await
    .map(|_| Value::Null)
    .map_err(docker_error)
}

/// Лог установки уходит тем же каналом, что консоль: панель показывает его в
/// том же окне, а не оставляет пользователя смотреть на крутилку десять минут.
///
/// Без ожидания места в очереди: пока мастера нет, она полна, и ожидание
/// останавливало саму установку на первой же строке лога.
fn spawn_install_log(
    events: mpsc::Sender<NodeEvent>,
    mut rx: mpsc::Receiver<String>,
    server: Uuid,
) {
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            let _ = events.try_send(NodeEvent::InstallLog {
                server,
                lines: vec![line],
            });
        }
    });
}

impl Daemon {
    pub fn layout(&self, server: Uuid) -> Layout {
        Layout::new(self.cfg.server_dir(server))
    }

    /// Гарантировать соответствие порта в `server.properties` выделенному порту.
    pub async fn ensure_server_port(&self, server: Uuid) {
        let layout = self.layout(server);
        let port = match layout.read_primary_port() {
            Some(p) => Some(p),
            None => self.engine.primary_port(server).await,
        };
        if let Some(port) = port {
            let _ = layout.write_primary_port(port);
            let path = layout.root.join("server.properties");
            if let Err(e) = crate::server::props::ensure_port(&path, port) {
                tracing::warn!(%server, port, error = %e, "не удалось выставить порт в server.properties");
            } else {
                tracing::info!(%server, port, "порт в server.properties синхронизирован");
            }
        }
    }
}
