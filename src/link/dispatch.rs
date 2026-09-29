// Файл превышает 150 строк: диспетчер операций — таблица «запрос → действие», и разрезать её значит потерять её вид.
//! Что делать с каждой операцией мастера.

use schema::noded::{NodeEvent, NodeOp, OpError, PowerAction, PowerState, TicketGrant, TicketMode};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::docker::{create::CreateArgs, Engine};
use crate::fs;
use crate::server::{install, layout::Layout, registry::Registry, supervisor};
use crate::Daemon;

/// Результат операции: то, что уедет в `Reply.data`.
pub type OpResult = std::result::Result<Value, OpError>;

pub async fn handle(daemon: &Daemon, op: NodeOp) -> OpResult {
    match op {
        NodeOp::NodeInfo => Ok(json!({ "version": env!("CARGO_PKG_VERSION") })),

        NodeOp::ImagePull { image } => daemon
            .engine
            .pull_image(&image)
            .await
            .map(|_| Value::Null)
            .map_err(|e| OpError::new("image_pull_failed", e.to_string())),

        NodeOp::ServerCreate { server, spec } | NodeOp::ServerUpdate { server, spec } => {
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
                let _ =
                    crate::server::props::ensure_port(&layout.root.join("server.properties"), port);
            }

            // Чем запускать, решает диск, а не карточка сервера: имя мог
            // сменить установщик. Найденное уезжает в ответе — мастеру надо
            // запомнить его, иначе следующая пересборка начнёт с того же.
            let mut spec = *spec;
            spec.jar = install::resolve_entry_point(&layout.root, &spec.jar);
            let spec = Box::new(spec);

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

        NodeOp::ServerDelete { server, keep_files } => {
            if daemon.engine.exists(server).await {
                daemon.engine.remove(server).await.ok();
            }
            daemon.registry.forget(server);
            daemon.quota.forget(server);
            if !keep_files {
                let dir = daemon.cfg.server_dir(server);
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)
                        .map_err(|e| OpError::new("docker_failed", e.to_string()))?;
                }
            }
            Ok(Value::Null)
        }

        NodeOp::Install { server, install } => {
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

        NodeOp::Power { server, action } => power(daemon, server, action).await,

        NodeOp::Command { server, line } => {
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

        NodeOp::ConsoleAttach { server } => attach(daemon, server).await,
        NodeOp::ConsoleDetach { .. } => Ok(Value::Null),

        NodeOp::FsList { server, path } => {
            fs::list::list_dir(&daemon.cfg.server_dir(server), &path)
                .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
                .map_err(fs_error)
        }

        NodeOp::FsRead {
            server,
            path,
            max_bytes,
        } => fs::io::read_text(&daemon.cfg.server_dir(server), &path, max_bytes)
            .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
            .map_err(fs_error),

        NodeOp::FsWrite {
            server,
            path,
            content,
        } => {
            let root = daemon.cfg.server_dir(server);
            // Проверка до записи: отказать после того, как файл лёг, уже поздно.
            daemon
                .quota
                .check(server, &root, content.len() as u64)
                .await
                .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;

            fs::io::write_text(&root, &path, &content)
                .map(|_| {
                    daemon.quota.add(server, content.len() as u64);
                    Value::Null
                })
                .map_err(fs_error)
        }

        NodeOp::FsDelete { server, paths } => {
            let root = daemon.cfg.server_dir(server);
            for path in &paths {
                fs::io::delete(&root, path).map_err(fs_error)?;
            }
            // Пересчитываем сразу: иначе освобождённое место «появится» только
            // через минуту, и уборка ради заливки выглядела бы бесполезной.
            daemon.quota.measure(server, &root).await;
            Ok(Value::Null)
        }

        NodeOp::FsMkdir { server, path } => fs::io::mkdir(&daemon.cfg.server_dir(server), &path)
            .map(|_| Value::Null)
            .map_err(fs_error),

        NodeOp::FsRename { server, from, to } => {
            fs::io::rename(&daemon.cfg.server_dir(server), &from, &to)
                .map(|_| Value::Null)
                .map_err(fs_error)
        }

        NodeOp::FsPull {
            server,
            url,
            sha1,
            dest,
        } => {
            let root = daemon.cfg.server_dir(server);
            let path = fs::resolve(&root, &dest).map_err(|e| fs_error(e.into()))?;
            daemon
                .master
                .download(&url, &path, Some(&sha1), None)
                .await
                .map(|bytes| json!({ "bytes": bytes }))
                .map_err(|e| OpError::new("upstream_failed", e.to_string()))
        }

        // Упаковка и распаковка идут в блокирующем пуле: архив каталога с
        // модами — это минуты, и на рабочем потоке рантайма они стоят консоли
        // всем серверам ноды.
        NodeOp::FsArchive {
            server,
            paths,
            dest,
        } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::archive::pack(&root, &paths, &dest))
                .await
                .map(|count| json!({ "count": count }))
                .map_err(fs_error)
        }

        NodeOp::FsUnarchive {
            server,
            path,
            dest_dir,
        } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::archive::unpack(&root, &path, &dest_dir))
                .await
                .map(|count| json!({ "count": count }))
                .map_err(fs_error)
        }

        NodeOp::SleepPlaceholder {
            server,
            port,
            listing,
            enable,
        } => {
            if !enable {
                // `remove` отдаёт держатель, и его `Drop` отпускает порт:
                // контейнеру он сейчас понадобится.
                daemon.placeholders.remove(&server);
                return Ok(Value::Null);
            }

            // Повторное включение не поднимает второй слушатель на том же
            // порту: мастер может прислать его снова после переподключения.
            if daemon.placeholders.contains_key(&server) {
                return Ok(Value::Null);
            }

            let engine = daemon.engine.clone();
            let registry = daemon.registry.clone();
            let events = daemon.events.clone();
            let placeholders = daemon.placeholders.clone();
            let gate = daemon.wake_gate.clone();

            let held = crate::server::waker::hold(
                server,
                port,
                *listing,
                std::sync::Arc::new(move |server: Uuid| {
                    // Стучать в порт может кто угодно, поэтому здесь два рубежа.
                    // Первый — частота, и он же защёлка: проверка памяти ниже
                    // асинхронная, плашка на порту всё ещё отвечает, и без
                    // отметки времени пачка стуков дала бы пачку запусков.
                    if !gate.allow(server) {
                        tracing::debug!(%server, "побудка отклонена: слишком часто");
                        return;
                    }
                    let engine = engine.clone();
                    let registry = registry.clone();
                    let events = events.clone();
                    let placeholders = placeholders.clone();
                    tokio::spawn(async move {
                        // Второй рубеж — память. Переподписка ноды рассчитана на
                        // то, что не всё работает разом, а поднять всё разом
                        // может любой, кто обойдёт порты спящих серверов.
                        if let Some(need) = engine.memory_limit_mb(server).await {
                            let free = crate::metrics::available_memory_mb();
                            if free < need {
                                tracing::warn!(
                                    %server, need, free,
                                    "побудка отклонена: на ноде нет столько памяти"
                                );
                                return;
                            }
                        }
                        // Порт отпускается перед запуском, но только когда
                        // запуск действительно будет: отпустить и отказать —
                        // значит убрать и плашку, и сервер из списка.
                        placeholders.remove(&server);
                        registry.get_or_create(server).starting_on_purpose();
                        if let Err(e) = engine.start(server).await {
                            tracing::warn!(%server, error = %format!("{e:#}"), "разбудить не удалось");
                            return;
                        }
                        let _ = supervisor::attach(engine, registry, events, server).await;
                    });
                }),
            )
            .await
            .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;

            daemon.placeholders.insert(server, held);
            Ok(Value::Null)
        }

        NodeOp::ServerPing { port, .. } => {
            // Loopback: the container publishes its port on the node, and the
            // game port is often closed to everyone but the machine itself.
            let pong = crate::server::ping::ping("127.0.0.1", port).await;
            Ok(serde_json::to_value(pong).unwrap_or(Value::Null))
        }

        NodeOp::ServerCloneFiles {
            server,
            from,
            include_worlds,
        } => {
            let to = daemon.cfg.server_dir(server);
            let source = daemon.cfg.server_dir(from);
            // Blocking copy on the blocking pool: a world is gigabytes, and
            // doing that on the async runtime stalls every other server's
            // console on this node.
            tokio::task::spawn_blocking(move || {
                crate::server::clone::run(&source, &to, include_worlds)
            })
            .await
            .map_err(|e| OpError::new("upstream_failed", format!("{e}")))?
            .map(|r| serde_json::json!({ "files": r.files, "bytes": r.bytes }))
            .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))
        }

        NodeOp::BuildSync {
            server,
            files,
            policy,
        } => {
            let root = daemon.cfg.server_dir(server);
            crate::server::sync::run(&daemon.master, &root, &files, &policy)
                .await
                .map(|report| serde_json::to_value(report).unwrap_or(Value::Null))
                .map_err(|e| OpError::new("upstream_failed", format!("{e:#}")))
        }

        NodeOp::BackupCreate {
            server,
            backup,
            ignore,
            upload,
            ..
        } => {
            let root = daemon.cfg.server_dir(server);
            let dest = crate::backup::archive_path(&root, backup);
            let r = crate::backup::create::create(&daemon.registry, &root, &dest, &ignore)
                .await
                .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;

            if let Some(target) = upload {
                crate::backup::remote::upload(&dest, &target)
                    .await
                    .map_err(|e| {
                        OpError::new(
                            "upstream_failed",
                            format!("заливка в хранилище не удалась: {e:#}"),
                        )
                    })?;
            }

            Ok(json!({ "backup": backup, "bytes": r.bytes, "sha256": r.sha256 }))
        }

        NodeOp::BackupRestore {
            server,
            backup,
            download,
        } => {
            let root = daemon.cfg.server_dir(server);
            let archive = crate::backup::archive_path(&root, backup);
            if !archive.exists() {
                if let Some(source) = download {
                    crate::backup::remote::download(&archive, &source)
                        .await
                        .map_err(|e| {
                            OpError::new(
                                "upstream_failed",
                                format!("загрузка из хранилища не удалась: {e:#}"),
                            )
                        })?;
                }
            }
            crate::backup::restore::restore(
                &daemon.engine,
                &daemon.registry,
                server,
                &root,
                &archive,
            )
            .await
            .map(|_| Value::Null)
            .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))
        }

        NodeOp::BackupDelete { server, backup } => {
            let root = daemon.cfg.server_dir(server);
            let archive = crate::backup::archive_path(&root, backup);
            if archive.exists() {
                std::fs::remove_file(&archive)
                    .map_err(|e| OpError::new("docker_failed", e.to_string()))?;
            }
            Ok(Value::Null)
        }

        // Тикет выписывается на уже разрешённый путь: проверить права в момент
        // самой передачи нечем — у браузера ни сессии, ни токена ноды нет.
        NodeOp::FsTicket {
            server,
            path,
            mode,
            ttl_secs,
        } => {
            let root = daemon.cfg.server_dir(server);
            let resolved = match mode {
                TicketMode::Download => fs::resolve_existing(&root, &path),
                TicketMode::Upload => fs::resolve(&root, &path),
            }
            .map_err(|e| fs_error(e.into()))?;
            if fs::is_hidden(&path) {
                return Err(OpError::new("panel_path_escapes", "служебный каталог"));
            }

            let name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            Ok(grant(daemon, server, resolved, mode, name, ttl_secs))
        }

        NodeOp::BackupTicket {
            server,
            backup,
            ttl_secs,
        } => {
            let root = daemon.cfg.server_dir(server);
            let archive = crate::backup::archive_path(&root, backup);
            if !archive.exists() {
                return Err(OpError::new("not_found", "бэкапа нет на ноде"));
            }
            Ok(grant(
                daemon,
                server,
                archive,
                TicketMode::Download,
                format!("{backup}.tar.gz"),
                ttl_secs,
            ))
        }

        NodeOp::SftpReload => Ok(Value::Null),
    }
}

/// Выписать тикет и собрать ответ.
///
/// Публичный адрес знает только оператор ноды: за NAT его нет, и тогда `url`
/// пустой — мастер поймёт, что байты придётся прокинуть через себя.
fn grant(
    daemon: &Daemon,
    server: Uuid,
    path: std::path::PathBuf,
    mode: TicketMode,
    filename: String,
    ttl_secs: u64,
) -> Value {
    let token = daemon
        .http_tickets
        .issue(server, path, mode, filename, ttl_secs);

    let url = daemon
        .cfg
        .http
        .public_url
        .as_deref()
        .map(|base| format!("{}/transfer/{token}", base.trim_end_matches('/')));

    let expires_at = chrono::Utc::now().timestamp() + ttl_secs.clamp(30, 3600) as i64;
    serde_json::to_value(TicketGrant {
        ticket: token,
        url,
        expires_at,
    })
    .unwrap_or(Value::Null)
}

async fn power(daemon: &Daemon, server: Uuid, action: PowerAction) -> OpResult {
    let engine = &daemon.engine;
    // Намерение человека важнее автоподъёма: без этой пометки присмотр поднял
    // бы сервер, который только что погасили — в том числе приостановленный,
    // который мастер гасит этой же командой.
    let handle = daemon.registry.get_or_create(server);
    match action {
        PowerAction::Start | PowerAction::Restart => {
            handle.starting_on_purpose();
            daemon.ensure_server_port(server).await;
        }
        PowerAction::Stop | PowerAction::Kill => handle.stopping_on_purpose(),
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
            engine.start(server).await.map_err(docker_error)?;
            return attach(daemon, server).await;
        }
    };
    result.map(|_| Value::Null).map_err(docker_error)
}

async fn attach(daemon: &Daemon, server: Uuid) -> OpResult {
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
fn spawn_install_log(
    events: mpsc::Sender<NodeEvent>,
    mut rx: mpsc::Receiver<String>,
    server: Uuid,
) {
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            let _ = events
                .send(NodeEvent::InstallLog {
                    server,
                    lines: vec![line],
                })
                .await;
        }
    });
}

/// Выполнить блокирующую работу вне рабочих потоков рантайма.
async fn blocking<T, F>(work: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("задача не выполнена: {e}")))
}

fn docker_error(e: anyhow::Error) -> OpError {
    OpError::new("docker_failed", format!("{e:#}"))
}

fn fs_error(e: anyhow::Error) -> OpError {
    // Побег из каталога — отдельный код: его показывают человеку иначе, чем
    // «файл не найден», и по нему же видно попытку в логе.
    let text = format!("{e:#}");
    if text.contains("выходит за каталог") {
        OpError::new("panel_path_escapes", text)
    } else {
        OpError::new("not_found", text)
    }
}

/// Состояние всех известных серверов — для периодического отчёта.
pub async fn poll_states(engine: &Engine, registry: &Registry) -> Vec<NodeEvent> {
    let mut events = Vec::new();
    for server in registry.ids() {
        if let Ok((power, exit_code, uptime)) = engine.state(server).await {
            let ready = registry
                .get(server)
                .map(|h| h.state().ready)
                .unwrap_or(false);
            if let Some(handle) = registry.get(server) {
                handle.set_power(power);
            }
            events.push(NodeEvent::ServerState {
                server,
                state: power,
                ready: ready && power == PowerState::Running,
                uptime_secs: uptime,
                exit_code,
            });
        }
    }
    events
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
