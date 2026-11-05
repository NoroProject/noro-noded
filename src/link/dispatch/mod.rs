// Файл превышает 150 строк: таблица «операция → действие» целиком, вместе с замком тяжёлых операций и опросом состояний.
//! Что делать с каждой операцией мастера.
//!
//! Здесь — только таблица: какая операция куда идёт. Сами действия лежат по
//! соседству, по одному файлу на тему, — таблица прежде росла вместе с ними и
//! перевалила за девятьсот строк.

mod backup;
mod files;
mod server;
mod sleep;

use schema::noded::{NodeEvent, NodeOp, OpError, PowerState};
use serde_json::Value;
use uuid::Uuid;

use crate::docker::Engine;
use crate::server::registry::Registry;
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
        // Удаление снимка — это сборка мусора в хранилище кусков, и идти
        // поверх упаковки, которая пишет туда же, она не должна.
        NodeOp::BackupDelete { server, .. } => (server, "удаление бэкапа"),
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
            server::apply_spec(daemon, server, spec).await
        }

        NodeOp::ServerDelete { server, keep_files } => {
            server::delete(daemon, server, keep_files).await
        }

        NodeOp::Install { server, install } => server::install(daemon, server, install).await,

        NodeOp::ServerPackApply { server, pack } => server::pack_apply(daemon, server, pack).await,

        NodeOp::Power { server, action } => server::power(daemon, server, action).await,

        NodeOp::Command { server, line } => server::command(daemon, server, line).await,

        NodeOp::ConsoleAttach { server } => server::attach(daemon, server).await,
        NodeOp::ConsoleDetach { .. } => Ok(Value::Null),

        NodeOp::FsList { server, path } => files::list(daemon, server, path).await,

        NodeOp::FsRead {
            server,
            path,
            max_bytes,
        } => files::read(daemon, server, path, max_bytes).await,

        NodeOp::FsWrite {
            server,
            path,
            content,
        } => files::write(daemon, server, path, content).await,

        NodeOp::FsDelete { server, paths } => files::delete(daemon, server, paths).await,

        NodeOp::FsMkdir { server, path } => files::mkdir(daemon, server, path).await,

        NodeOp::FsRename { server, from, to } => files::rename(daemon, server, from, to).await,

        NodeOp::FsCopy { server, from, to } => files::copy(daemon, server, from, to).await,

        NodeOp::FsPull {
            server,
            url,
            sha1,
            dest,
        } => files::pull(daemon, server, url, sha1, dest).await,

        NodeOp::FsArchive {
            server,
            paths,
            dest,
        } => files::archive(daemon, server, paths, dest).await,

        NodeOp::FsUnarchive {
            server,
            path,
            dest_dir,
        } => files::unarchive(daemon, server, path, dest_dir).await,

        NodeOp::SleepPlaceholder {
            server,
            port,
            listing,
            enable,
        } => sleep::sleep_placeholder(daemon, server, port, listing, enable).await,

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
        } => server::clone_files(daemon, server, from, include_worlds).await,

        NodeOp::BuildSync {
            server,
            files,
            policy,
        } => server::build_sync(daemon, server, files, policy).await,

        NodeOp::BackupCreate {
            server,
            backup,
            ignore,
            upload,
            quota_mb,
            ..
        } => backup::create(daemon, server, backup, ignore, upload, quota_mb).await,

        NodeOp::BackupRestore {
            server,
            backup,
            download,
            paths,
        } => backup::restore(daemon, server, backup, download, paths).await,

        NodeOp::BackupBrowse {
            server,
            backup,
            path,
        } => backup::browse(daemon, server, backup, path).await,

        NodeOp::BackupDelete { server, backup } => backup::delete(daemon, server, backup).await,

        NodeOp::FsTicket {
            server,
            path,
            mode,
            ttl_secs,
        } => files::ticket(daemon, server, path, mode, ttl_secs).await,

        NodeOp::BackupTicket {
            server,
            backup,
            ttl_secs,
        } => backup::ticket(daemon, server, backup, ttl_secs).await,

        NodeOp::CrashReport { server, crash } => {
            let store = daemon.registry.crashes.clone();
            blocking(move || store.read(server, crash.as_deref()))
                .await
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .map_err(|e| OpError::new("not_found", format!("{e:#}")))
        }

        NodeOp::SftpReload => Ok(Value::Null),

        NodeOp::NetworkGroups { groups } => daemon
            .isolation
            .set_groups(&daemon.engine, groups)
            .await
            .map(|_| serde_json::to_value(daemon.isolation.diag()).unwrap_or(Value::Null))
            .map_err(|e| OpError::new("docker_failed", format!("{e:#}"))),

        NodeOp::ServerLimits { server, disk_mb } => server::limits(daemon, server, disk_mb).await,
    }
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
