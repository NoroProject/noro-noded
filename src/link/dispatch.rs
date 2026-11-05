// Файл превышает 150 строк: диспетчер операций — таблица «запрос → действие», и разрезать её значит потерять её вид.
//! Что делать с каждой операцией мастера.

use schema::noded::{NodeEvent, NodeOp, OpError, PowerAction, PowerState, TicketGrant, TicketMode};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::docker::{create::CreateArgs, Engine};
use crate::fs;
use crate::http::tickets::Target;
use crate::server::{install, layout::Layout, registry::Registry, supervisor};
use crate::Daemon;

/// Результат операции: то, что уедет в `Reply.data`.
pub type OpResult = std::result::Result<Value, OpError>;

pub async fn handle(daemon: &Daemon, op: NodeOp) -> OpResult {
    let _busy = match heavy(&op) {
        Some((server, what)) => Some(daemon.busy.try_take(server, what).map_err(|running| {
            OpError::new(
                "server_busy",
                format!("на сервере уже идёт другая операция: {running}"),
            )
        })?),
        None => None,
    };
    run(daemon, op).await
}

/// Операции, которые меняют сервер целиком и не должны идти вперемешку.
fn heavy(op: &NodeOp) -> Option<(Uuid, &'static str)> {
    let (server, what) = match op {
        NodeOp::ServerCreate { server, .. } => (server, "создание"),
        NodeOp::ServerUpdate { server, .. } => (server, "пересборка"),
        NodeOp::ServerDelete { server, .. } => (server, "удаление"),
        NodeOp::Install { server, .. } => (server, "установка"),
        NodeOp::ServerPackApply { server, .. } => (server, "установка пака"),
        NodeOp::BuildSync { server, .. } => (server, "синхронизация сборки"),
        NodeOp::BackupCreate { server, .. } => (server, "бэкап"),
        NodeOp::BackupRestore { server, .. } => (server, "восстановление"),
        NodeOp::ServerCloneFiles { server, .. } => (server, "клонирование"),
        _ => return None,
    };
    Some((*server, what))
}

async fn run(daemon: &Daemon, op: NodeOp) -> OpResult {
    match op {
        // Версия — как и прежде, остальное — для разбора «что с нодой».
        NodeOp::NodeInfo => Ok(crate::diag::report(daemon)),

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

        NodeOp::ServerDelete { server, keep_files } => {
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
                if dir.exists() {
                    blocking(move || Ok(std::fs::remove_dir_all(&dir)?))
                        .await
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

        NodeOp::ServerPackApply { server, pack } => {
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

        // Файловые операции — в блокирующем пуле, как и тяжёлые ниже: каталог
        // на десятки тысяч файлов, удаление мира или медленный диск стоят
        // секунд, и на рабочем потоке рантайма они останавливали консоли и
        // связь с мастером для всей ноды.
        NodeOp::FsList { server, path } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::list::list_dir(&root, &path))
                .await
                .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
                .map_err(fs_error)
        }

        NodeOp::FsRead {
            server,
            path,
            max_bytes,
        } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::io::read_text(&root, &path, max_bytes))
                .await
                .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
                .map_err(fs_error)
        }

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

            let size = content.len() as u64;
            blocking(move || {
                let written = fs::io::write_text(&root, &path, &content)?;
                if let Err(e) = crate::server::layout::chown_path(&written) {
                    tracing::warn!(server = %server, error = %e, "config file owner not corrected");
                }
                Ok(())
            })
            .await
            .map(|_| {
                daemon.quota.add(server, size);
                Value::Null
            })
            .map_err(fs_error)
        }

        NodeOp::FsDelete { server, paths } => {
            let root = daemon.cfg.server_dir(server);
            let target = root.clone();
            let deleted = blocking(move || {
                for path in &paths {
                    fs::io::delete(&target, path)?;
                }
                Ok(())
            })
            .await;
            // Пересчитываем сразу, и после частичной неудачи тоже: иначе
            // освобождённое место «появится» только через минуту, и уборка
            // ради заливки выглядела бы бесполезной.
            daemon.quota.measure(server, &root).await;
            deleted.map(|_| Value::Null).map_err(fs_error)
        }

        NodeOp::FsMkdir { server, path } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::io::mkdir(&root, &path))
                .await
                .map(|_| Value::Null)
                .map_err(fs_error)
        }

        NodeOp::FsRename { server, from, to } => {
            let root = daemon.cfg.server_dir(server);
            blocking(move || fs::io::rename(&root, &from, &to))
                .await
                .map(|_| Value::Null)
                .map_err(fs_error)
        }

        NodeOp::FsCopy { server, from, to } => {
            let root = daemon.cfg.server_dir(server);
            // Копия занимает место ровно как заливка: без проверки квоты
            // «скопировать world» обходит лимит диска в два щелчка.
            let (src_root, source) = (root.clone(), from.clone());
            let size = blocking(move || {
                Ok(fs::resolve_existing(&src_root, &source)
                    .map(|src| fs::quota::dir_size(&src))
                    .unwrap_or(0))
            })
            .await
            .unwrap_or(0);
            daemon
                .quota
                .check(server, &root, size)
                .await
                .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;
            let measured = root.clone();
            let result = blocking(move || fs::io::copy(&root, &from, &to))
                .await
                .map(|count| json!({ "count": count }))
                .map_err(fs_error);
            daemon.quota.measure(server, &measured).await;
            result
        }

        NodeOp::FsPull {
            server,
            url,
            sha1,
            dest,
        } => {
            let root = daemon.cfg.server_dir(server);
            let path = fs::resolve(&root, &dest).map_err(|e| fs_error(e.into()))?;
            let cached = path.clone();
            let have = blocking(move || {
                let len = std::fs::symlink_metadata(&cached)?.len();
                Ok((len, crate::server::sync::file_sha1(&cached)?))
            })
            .await;
            if let Ok((len, hash)) = have {
                if hash.eq_ignore_ascii_case(&sha1) {
                    return Ok(json!({ "bytes": len, "cached": true }));
                }
            }
            // Заливка по ссылке занимает место так же, как через браузер.
            daemon
                .quota
                .check(server, &root, 0)
                .await
                .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;
            let pulled = daemon
                .master
                .download(&url, &path, Some(&sha1), None)
                .await
                .map(|bytes| json!({ "bytes": bytes }))
                .map_err(|e| OpError::new("upstream_failed", e.to_string()));
            if pulled.is_ok() {
                if let Err(e) = crate::server::layout::chown_path(&path) {
                    tracing::warn!(%server, error = %e, "скачанный файл остался с чужим владельцем");
                }
            }
            daemon.quota.measure(server, &root).await;
            pulled
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
            // Архив — копия своих источников, сжатая или нет: без проверки
            // «упаковать world» обходил квоту так же, как копирование.
            let (src_root, sources) = (root.clone(), paths.clone());
            let size = blocking(move || {
                Ok(sources
                    .iter()
                    .filter_map(|p| fs::resolve_existing(&src_root, p).ok())
                    .map(|p| fs::quota::dir_size(&p))
                    .sum::<u64>())
            })
            .await
            .unwrap_or(0);
            daemon
                .quota
                .check(server, &root, size)
                .await
                .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;
            let measured = root.clone();
            let result = blocking(move || fs::archive::pack(&root, &paths, &dest))
                .await
                .map(|count| json!({ "count": count }))
                .map_err(fs_error);
            daemon.quota.measure(server, &measured).await;
            result
        }

        NodeOp::FsUnarchive {
            server,
            path,
            dest_dir,
        } => {
            let root = daemon.cfg.server_dir(server);
            let usage = daemon.quota.usage(server, &root).await;
            let server_room =
                (usage.limit_bytes > 0).then(|| usage.limit_bytes.saturating_sub(usage.used_bytes));
            // Распаковка упирается и в квоту сервера, и в запас диска ноды.
            let budget = match (server_room, daemon.quota.node_room()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let measured = root.clone();
            let result =
                blocking(move || fs::archive::unpack_within(&root, &path, &dest_dir, budget))
                    .await
                    .map(|count| json!({ "count": count }))
                    .map_err(|e| {
                        if e.to_string().contains("не помещается") {
                            OpError::new("panel_disk_full", e.to_string())
                        } else {
                            fs_error(e)
                        }
                    });
            daemon.quota.measure(server, &measured).await;
            result
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
            // Размер архива заранее неизвестен, но начинать его на ноде, где
            // свободен уже только запас, значит доесть место у живых серверов.
            daemon
                .quota
                .node_fits(0)
                .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;
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
            if std::fs::symlink_metadata(&archive).is_ok() {
                fs::nofollow::remove(&archive)
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
            // Проверка сейчас — чтобы панель сразу сказала «нет такого файла»;
            // при самой передаче путь разрешается ещё раз.
            match mode {
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
            Ok(grant(
                daemon,
                server,
                Target::File(path),
                mode,
                name,
                ttl_secs,
            ))
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
                Target::Backup(backup),
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
    target: Target,
    mode: TicketMode,
    filename: String,
    ttl_secs: u64,
) -> Value {
    let token = daemon
        .http_tickets
        .issue(server, target, mode, filename, ttl_secs);

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
            let stopped_by_user = registry
                .get(server)
                .map(|h| h.state().stopped_on_purpose)
                .unwrap_or(false);
            events.push(NodeEvent::ServerState {
                server,
                state: power,
                ready: ready && power == PowerState::Running,
                uptime_secs: uptime,
                exit_code,
                stopped_by_user,
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
