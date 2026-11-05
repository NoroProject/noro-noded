// Файл превышает 150 строк: все операции бэкапа сервера — упаковка, восстановление, удаление, просмотр и выдача — над одним каталогом.
//! Бэкапы: упаковка, восстановление, удаление, просмотр, выдача на скачивание.

use schema::noded::{BackupDeleted, BackupFormat, BackupInfo, BackupUpload, OpError, TicketMode};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

use super::files::grant;
use super::{blocking, OpResult};
use crate::backup::create::{OverBudget, Target as PackTarget};
use crate::backup::repo::Repo;
use crate::backup::restore::Source;
use crate::backup::store::{self, Kept};
use crate::fs;
use crate::http::tickets::Target;
use crate::Daemon;

const MIB: u64 = 1024 * 1024;

/// Каталог бэкапов сервера — созданный, сверенный и с перенесёнными из
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

async fn usage(dir: &std::path::Path) -> u64 {
    let measured = dir.to_path_buf();
    blocking(move || Ok(store::usage(&measured)))
        .await
        .unwrap_or(0)
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

    let used = usage(&dir).await;
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

    let (target, format) = if daemon.cfg.backups.incremental {
        (
            PackTarget::Snapshot {
                repo: dir.clone(),
                backup,
            },
            BackupFormat::Chunks,
        )
    } else {
        (
            PackTarget::Archive(store::archive_path(&dir, backup)),
            BackupFormat::Tar,
        )
    };
    let r = crate::backup::create::create(&daemon.registry, &root, target, &ignore, limit)
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

    if let Some(target) = &upload {
        let pushed = match format {
            BackupFormat::Tar => {
                crate::backup::remote::upload(&store::archive_path(&dir, backup), target).await
            }
            BackupFormat::Chunks => {
                crate::backup::remote::upload_snapshot(dir.clone(), backup, target).await
            }
        };
        pushed.map_err(|e| {
            OpError::new(
                "upstream_failed",
                format!("заливка в хранилище не удалась: {e:#}"),
            )
        })?;
    }

    Ok(reply(&BackupInfo {
        backup,
        bytes: r.bytes,
        sha256: r.sha256,
        storage_path: upload.map(|u| u.path),
        format,
        source_bytes: r.source_bytes,
        // Замер заново, а не `used + bytes`: упаковка снимка могла и убрать
        // куски, брошенные прошлой оборванной упаковкой.
        backups_bytes: Some(usage(&dir).await),
    }))
}

fn reply(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
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
    paths: Vec<String>,
) -> OpResult {
    let root = daemon.cfg.server_dir(server);
    let dir = backup_dir(daemon, server).await?;
    let source = match store::find(&dir, backup) {
        Kept::Archive(path) => Source::Archive(path),
        Kept::Snapshot => Source::Snapshot { repo: dir, backup },
        // В хранилище бэкап лежит собранным архивом при любом виде на ноде.
        Kept::Missing => {
            let archive = store::archive_path(&dir, backup);
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
            Source::Archive(archive)
        }
    };
    crate::backup::restore::restore(
        &daemon.engine,
        &daemon.registry,
        server,
        &root,
        source,
        paths,
    )
    .await
    .map(|_| Value::Null)
    .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))
}

pub async fn delete(daemon: &Daemon, server: Uuid, backup: Uuid) -> OpResult {
    let dir = backup_dir(daemon, server).await?;
    let measured = dir.clone();
    blocking(move || {
        if let Kept::Archive(archive) = store::find(&dir, backup) {
            fs::nofollow::remove(&archive)?;
        }
        let repo = Repo::at(&dir);
        if repo.remove_snapshot(backup)? {
            let done = crate::backup::gc::collect(&repo)?;
            tracing::info!(%server, %backup, removed = done.removed, freed = done.freed, "снимок удалён");
        }
        Ok(())
    })
    .await
    .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;
    Ok(reply(&BackupDeleted {
        backups_bytes: Some(usage(&measured).await),
    }))
}

pub async fn ticket(daemon: &Daemon, server: Uuid, backup: Uuid, ttl_secs: u64) -> OpResult {
    let dir = backup_dir(daemon, server).await?;
    if matches!(store::find(&dir, backup), Kept::Missing) {
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

pub async fn browse(daemon: &Daemon, server: Uuid, backup: Uuid, path: String) -> OpResult {
    let dir = backup_dir(daemon, server).await?;
    match store::find(&dir, backup) {
        Kept::Snapshot => {}
        // Заглянуть в tar.gz можно, только распаковав его целиком, — для
        // мира это минуты ради одного списка.
        Kept::Archive(_) => {
            return Err(OpError::new(
                "panel_backup_not_browsable",
                "полный архив не просматривается",
            ))
        }
        Kept::Missing => return Err(OpError::new("not_found", "бэкапа нет на ноде")),
    }
    blocking(move || {
        let manifest = Repo::at(&dir).manifest(backup)?;
        Ok(crate::backup::browse::list(&manifest, &path))
    })
    .await
    .map(|listing| reply(&listing))
    .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))
}
