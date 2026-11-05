//! Бэкапы: упаковка, восстановление, удаление, выдача на скачивание.

use schema::noded::{BackupUpload, OpError, TicketMode};
use serde_json::{json, Value};
use std::path::PathBuf;
use uuid::Uuid;

use super::files::grant;
use super::{blocking, OpResult};
use crate::backup::create::OverBudget;
use crate::backup::store;
use crate::fs;
use crate::http::tickets::Target;
use crate::Daemon;

const MIB: u64 = 1024 * 1024;

/// Каталог архивов сервера — созданный, сверенный и с перенесёнными из
/// прежнего места архивами.
async fn backup_dir(daemon: &Daemon, server: Uuid) -> Result<PathBuf, OpError> {
    let dir = daemon.cfg.backup_dir(server);
    let root = daemon.cfg.server_dir(server);
    let prepared = dir.clone();
    blocking(move || store::prepare(&prepared, &root))
        .await
        .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;
    Ok(dir)
}

pub async fn create(
    daemon: &Daemon,
    server: Uuid,
    backup: Uuid,
    ignore: Vec<String>,
    upload: Option<Box<BackupUpload>>,
    quota_mb: u64,
) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    let dir = backup_dir(daemon, server).await?;
    // Размер архива заранее неизвестен, но начинать его на ноде, где
    // свободен уже только запас, значит доесть место у живых серверов.
    daemon
        .quota
        .node_fits(0)
        .map_err(|e| OpError::new("panel_disk_full", e.to_string()))?;

    let measured = dir.clone();
    let used = blocking(move || Ok(store::usage(&measured)))
        .await
        .unwrap_or(0);
    let quota = quota_mb.saturating_mul(MIB);
    if quota > 0 && used >= quota {
        return Err(quota_refusal(used, quota));
    }
    let quota_room = (quota > 0).then(|| quota - used);
    let node_room = daemon.quota.node_room();
    let limit = match (quota_room, node_room) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };

    let dest = store::archive_path(&dir, backup);
    let r = crate::backup::create::create(&daemon.registry, &root, &dest, &ignore, limit)
        .await
        .map_err(|e| match e.downcast_ref::<OverBudget>() {
            // Упёрлись в то из двух, что меньше: его и называем.
            Some(_) if quota_room.is_some_and(|q| node_room.is_none_or(|n| q <= n)) => {
                quota_refusal(used, quota)
            }
            Some(over) => OpError::new("panel_disk_full", over.to_string()),
            None => OpError::new("docker_failed", format!("{e:#}")),
        })?;
    daemon.quota.node_consume(r.bytes);

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

    Ok(json!({
        "backup": backup,
        "bytes": r.bytes,
        "sha256": r.sha256,
        "backups_bytes": used + r.bytes,
    }))
}

fn quota_refusal(used: u64, quota: u64) -> OpError {
    OpError::new(
        "panel_backup_quota",
        format!(
            "место под бэкапы кончилось: занято {} МБ из {} МБ",
            used / MIB,
            quota / MIB
        ),
    )
}

pub async fn restore(
    daemon: &Daemon,
    server: Uuid,
    backup: Uuid,
    download: Option<Box<BackupUpload>>,
) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    let archive = store::archive_path(&backup_dir(daemon, server).await?, backup);
    if std::fs::symlink_metadata(&archive).is_err() {
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
    crate::backup::restore::restore(&daemon.engine, &daemon.registry, server, &root, &archive)
        .await
        .map(|_| Value::Null)
        .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))
}

pub async fn delete(daemon: &Daemon, server: Uuid, backup: Uuid) -> OpResult {
    let archive = store::archive_path(&backup_dir(daemon, server).await?, backup);
    if std::fs::symlink_metadata(&archive).is_ok() {
        fs::nofollow::remove(&archive).map_err(|e| OpError::new("docker_failed", e.to_string()))?;
    }
    Ok(Value::Null)
}

pub async fn ticket(daemon: &Daemon, server: Uuid, backup: Uuid, ttl_secs: u64) -> OpResult {
    let archive = store::archive_path(&backup_dir(daemon, server).await?, backup);
    if std::fs::symlink_metadata(&archive).is_err() {
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
