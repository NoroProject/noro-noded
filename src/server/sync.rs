// Файл превышает 150 строк: скачивание, сверка и уборка — одна операция, и разрезать её значит потерять порядок.
//! Раскатка сборки в каталог сервера.
//!
//! Правило здесь одно и оно важнее скорости: **лишнее удаляется только внутри
//! управляемых каталогов**. Всё остальное — мир, логи, конфиги, которых нет в
//! сборке, — остаётся на месте и возвращается списком. Мир, потерянный синком,
//! извинениями не восстанавливается.

use anyhow::Result;
use schema::build::FileEntry;
use schema::noded::{SyncPolicy, SyncReport};
use sha1::Digest;
use std::collections::HashSet;
use std::path::Path;

use crate::link::master::MasterClient;

pub async fn run(
    master: &MasterClient,
    root: &Path,
    files: &[FileEntry],
    policy: &SyncPolicy,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let mut wanted: HashSet<String> = HashSet::with_capacity(files.len());

    for file in files {
        wanted.insert(file.path.clone());

        let path = match crate::fs::resolve(root, &file.path) {
            Ok(path) => path,
            Err(_) => {
                // Путь из сборки, выходящий за каталог сервера, — это либо
                // ошибка сборки, либо попытка. И то и другое надо показать, а
                // не молча пропустить.
                report.failed.push(file.path.clone());
                continue;
            }
        };

        if up_to_date(&path, file) {
            report.unchanged += 1;
            continue;
        }

        match master
            .download(&file.url, &path, Some(&file.sha1), None)
            .await
        {
            Ok(bytes) => {
                report.downloaded += 1;
                report.bytes += bytes;
                if file.executable {
                    set_executable(&path)?;
                }
            }
            Err(e) => {
                tracing::warn!(path = %file.path, error = %format!("{e:#}"), "файл сборки не встал");
                report.failed.push(file.path.clone());
            }
        }
    }

    prune(root, policy, &wanted, &mut report);
    crate::server::layout::chown_recursive(root)?;
    Ok(report)
}

/// Файл уже такой, как надо.
///
/// Размер сравнивается первым: он бесплатный, и на модпаке в четыреста файлов
/// это разница между секундой и минутой чтения диска.
fn up_to_date(path: &Path, file: &FileEntry) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != file.size {
        return false;
    }
    file_sha1(path).is_ok_and(|hash| hash.eq_ignore_ascii_case(&file.sha1))
}

/// Убрать из управляемых каталогов то, чего в сборке нет.
///
/// За их пределами не удаляется ничего: файл, который туда положил владелец,
/// панель показывает как «не из сборки», а не уносит.
fn prune(root: &Path, policy: &SyncPolicy, wanted: &HashSet<String>, report: &mut SyncReport) {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");

        if wanted.contains(&rel) || is_kept(&rel, policy) || is_agent(&rel) {
            continue;
        }
        if !in_managed_root(&rel, policy) {
            continue;
        }

        if policy.prune {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => report.deleted += 1,
                Err(e) => {
                    tracing::warn!(path = %rel, error = %e, "лишний файл не удалён");
                    report.failed.push(rel);
                }
            }
        } else {
            // Уборка выключена — просто показываем, что нашли лишнее.
            report.extra.push(rel);
        }
    }
}

fn in_managed_root(rel: &str, policy: &SyncPolicy) -> bool {
    policy
        .managed_roots
        .iter()
        .any(|root| rel == root || rel.starts_with(&format!("{root}/")))
}

/// Агент лежит в `mods/` или `plugins/` — ровно там, где синк убирает лишнее.
/// В сборке его нет и быть не должно, так что уборка сносила бы его при каждой
/// раскатке, а сервер после этого молча переставал отвечать мастеру.
fn is_agent(rel: &str) -> bool {
    rel == super::agent::rel_path("mods") || rel == super::agent::rel_path("plugins")
}

fn is_kept(rel: &str, policy: &SyncPolicy) -> bool {
    policy
        .keep
        .iter()
        .any(|keep| rel == keep || rel.starts_with(&format!("{keep}/")))
}

pub fn file_sha1(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha1::Sha1::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
