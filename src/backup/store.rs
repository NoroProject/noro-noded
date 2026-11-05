//! Где лежат архивы сервера и сколько они занимают.
//!
//! Каталог — `data_dir/backups/<server>/`, вне каталога сервера. Прежде архивы
//! лежали в `.noro/backups` внутри него, а он примонтирован в контейнер:
//! плагин мог удалить точки восстановления или подменить архив, из которого
//! потом восстанавливают испорченный мир. Оттуда они переносятся сюда — при
//! старте демона и при первом обращении к бэкапам сервера.
//!
//! Место под архивы считается отдельно от квоты диска сервера: у бэкапов свой
//! лимит, его задаёт мастер в каждой операции бэкапа.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::fs::nofollow;
use crate::server::layout::Layout;

/// Расширение архива. По нему же уборка отличает архивы от прочего.
const SUFFIX: &str = ".tar.gz";

pub fn archive_path(dir: &Path, backup: Uuid) -> PathBuf {
    dir.join(format!("{backup}{SUFFIX}"))
}

/// Подготовить каталог архивов сервера: создать сверенным и забрать в него
/// архивы, оставшиеся от прежнего места.
pub fn prepare(dir: &Path, server_root: &Path) -> Result<()> {
    let parent = dir.parent().context("у каталога бэкапов нет родителя")?;
    nofollow::ensure_dir(parent).context("не создать каталог бэкапов ноды")?;
    nofollow::ensure_dir(dir).context("не создать каталог бэкапов сервера")?;
    let moved = migrate_legacy(server_root, dir);
    if moved > 0 {
        tracing::info!(dir = %dir.display(), moved, "архивы перенесены из каталога сервера");
    }
    Ok(())
}

/// Перенести архивы из `.noro/backups` сервера. Возвращает, сколько перенесено.
///
/// Каталог-источник принадлежит процессу сервера, поэтому переносится только
/// то, что выглядит нашим архивом (`<uuid>.tar.gz`) и является обычным файлом;
/// переименование переносит саму запись и по ссылке не идёт.
pub fn migrate_legacy(server_root: &Path, dir: &Path) -> usize {
    let legacy = Layout::new(server_root.to_path_buf()).legacy_backups_dir();
    // Нет каталога или на его месте ссылка — переносить нечего.
    let Ok(source) = nofollow::Dir::open(&legacy) else {
        return 0;
    };
    let Ok(entries) = source.read_dir() else {
        return 0;
    };

    let mut moved = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(backup) = name.to_str().and_then(backup_of) else {
            continue;
        };
        let from = legacy.join(&name);
        let is_file = std::fs::symlink_metadata(source.entry(&name))
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if !is_file {
            continue;
        }
        let to = archive_path(dir, backup);
        if std::fs::symlink_metadata(&to).is_ok() {
            // Архив уже на новом месте — старая копия лишняя.
            let _ = nofollow::remove_file(&from);
            continue;
        }
        match move_file(&from, &to) {
            Ok(()) => moved += 1,
            Err(e) => tracing::warn!(%backup, error = %e, "архив не перенесён"),
        }
    }

    // Пустой каталог убирается; непустой (чужие файлы) остаётся как есть.
    if let Ok(service) = nofollow::Dir::open(&Layout::new(server_root.to_path_buf()).service_dir())
    {
        let _ = std::fs::remove_dir(service.entry("backups"));
    }
    moved
}

/// Переименование, а между дисками — копия и удаление: каталог данных может
/// быть смонтирован по частям.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match nofollow::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            if let Err(e) = nofollow::copy_file(from, to) {
                let _ = nofollow::remove_file(to);
                return Err(e);
            }
            nofollow::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

/// `EXDEV` — переименование через границу файловых систем (одно и то же
/// число на Linux и macOS).
const EXDEV: i32 = 18;

/// Идентификатор бэкапа по имени архива.
fn backup_of(name: &str) -> Option<Uuid> {
    name.strip_suffix(SUFFIX)
        .and_then(|id| Uuid::parse_str(id).ok())
}

/// Сколько байт занимают бэкапы сервера: архивы, куски и манифесты — то, что
/// реально лежит на диске. С дедупликацией сумма размеров бэкапов говорит о
/// месте не больше, чем сумма размеров миров. Ссылки и прочее не считаются:
/// их здесь быть не должно, а прочитать по ним всё равно ничего не дадут.
pub fn usage(dir: &Path) -> u64 {
    if nofollow::Dir::open(dir).is_err() {
        return 0;
    }
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// В каком виде бэкап лежит на ноде.
pub enum Kept {
    Archive(PathBuf),
    Snapshot,
    Missing,
}

pub fn find(dir: &Path, backup: Uuid) -> Kept {
    let archive = archive_path(dir, backup);
    if std::fs::symlink_metadata(&archive).is_ok_and(|m| m.is_file()) {
        return Kept::Archive(archive);
    }
    if super::repo::Repo::at(dir).has_snapshot(backup) {
        return Kept::Snapshot;
    }
    Kept::Missing
}

#[cfg(all(test, unix))]
#[path = "store_tests.rs"]
mod tests;
