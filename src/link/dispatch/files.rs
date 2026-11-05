// Файл превышает 150 строк: каждая операция файлового менеджера — короткая функция, а вместе они одна тема.
//! Файловый менеджер панели и прямая передача.

use schema::noded::{OpError, TicketGrant, TicketMode};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{blocking, fs_error, OpResult};
use crate::fs;
use crate::http::tickets::Target;
use crate::Daemon;

// Файловые операции — в блокирующем пуле, как и тяжёлые ниже: каталог
// на десятки тысяч файлов, удаление мира или медленный диск стоят
// секунд, и на рабочем потоке рантайма они останавливали консоли и
// связь с мастером для всей ноды.
pub async fn list(daemon: &Daemon, server: Uuid, path: String) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    blocking(move || fs::list::list_dir(&root, &path))
        .await
        .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
        .map_err(fs_error)
}

pub async fn read(daemon: &Daemon, server: Uuid, path: String, max_bytes: u64) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    blocking(move || fs::io::read_text(&root, &path, max_bytes))
        .await
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .map_err(fs_error)
}

pub async fn write(daemon: &Daemon, server: Uuid, path: String, content: String) -> OpResult {
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

pub async fn delete(daemon: &Daemon, server: Uuid, paths: Vec<String>) -> OpResult {
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

pub async fn mkdir(daemon: &Daemon, server: Uuid, path: String) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    blocking(move || fs::io::mkdir(&root, &path))
        .await
        .map(|_| Value::Null)
        .map_err(fs_error)
}

pub async fn rename(daemon: &Daemon, server: Uuid, from: String, to: String) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    blocking(move || fs::io::rename(&root, &from, &to))
        .await
        .map(|_| Value::Null)
        .map_err(fs_error)
}

pub async fn copy(daemon: &Daemon, server: Uuid, from: String, to: String) -> OpResult {
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

pub async fn pull(
    daemon: &Daemon,
    server: Uuid,
    url: String,
    sha1: String,
    dest: String,
) -> OpResult {
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
pub async fn archive(daemon: &Daemon, server: Uuid, paths: Vec<String>, dest: String) -> OpResult {
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

pub async fn unarchive(daemon: &Daemon, server: Uuid, path: String, dest_dir: String) -> OpResult {
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
    let result = blocking(move || fs::archive::unpack_within(&root, &path, &dest_dir, budget))
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

// Тикет выписывается на уже разрешённый путь: проверить права в момент
// самой передачи нечем — у браузера ни сессии, ни токена ноды нет.
pub async fn ticket(
    daemon: &Daemon,
    server: Uuid,
    path: String,
    mode: TicketMode,
    ttl_secs: u64,
) -> OpResult {
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

/// Выписать тикет и собрать ответ.
///
/// Публичный адрес знает только оператор ноды: за NAT его нет, и тогда `url`
/// пустой — мастер поймёт, что байты придётся прокинуть через себя.
pub(super) fn grant(
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
