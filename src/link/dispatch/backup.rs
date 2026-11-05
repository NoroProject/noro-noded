//! Бэкапы: упаковка, восстановление, удаление, выдача на скачивание.

use schema::noded::{BackupUpload, OpError, TicketMode};
use serde_json::{json, Value};
use uuid::Uuid;

use super::files::grant;
use super::OpResult;
use crate::fs;
use crate::http::tickets::Target;
use crate::Daemon;

pub async fn create(
    daemon: &Daemon,
    server: Uuid,
    backup: Uuid,
    ignore: Vec<String>,
    upload: Option<Box<BackupUpload>>,
) -> OpResult {
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

pub async fn restore(
    daemon: &Daemon,
    server: Uuid,
    backup: Uuid,
    download: Option<Box<BackupUpload>>,
) -> OpResult {
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
    crate::backup::restore::restore(&daemon.engine, &daemon.registry, server, &root, &archive)
        .await
        .map(|_| Value::Null)
        .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))
}

pub async fn delete(daemon: &Daemon, server: Uuid, backup: Uuid) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    let archive = crate::backup::archive_path(&root, backup);
    if std::fs::symlink_metadata(&archive).is_ok() {
        fs::nofollow::remove(&archive).map_err(|e| OpError::new("docker_failed", e.to_string()))?;
    }
    Ok(Value::Null)
}

pub async fn ticket(daemon: &Daemon, server: Uuid, backup: Uuid, ttl_secs: u64) -> OpResult {
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
