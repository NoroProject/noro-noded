//! Перечисление каталога сервера.

use anyhow::Result;
use schema::noded::{DirEntry, DirListing};
use std::path::Path;

/// Содержимое каталога. Скрытое служебное не показывается вовсе: его нельзя ни
/// прочитать, ни переписать, и строка в списке только путала бы.
pub fn list_dir(root: &Path, rel: &str) -> Result<DirListing> {
    let dir = super::resolve(root, rel)?;
    let mut entries = Vec::new();

    for item in std::fs::read_dir(&dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let child_rel = if rel.is_empty() || rel == "." {
            name.clone()
        } else {
            format!("{}/{name}", rel.trim_end_matches('/'))
        };
        if super::is_hidden(&child_rel) {
            continue;
        }

        // symlink_metadata, а не metadata: битая ссылка не должна выкидывать
        // весь каталог из выдачи, а по ссылке мы всё равно не ходим.
        let meta = item
            .metadata()
            .or_else(|_| item.path().symlink_metadata())?;
        let link = std::fs::symlink_metadata(item.path())
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);

        entries.push(DirEntry {
            name,
            path: child_rel,
            dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            symlink: link,
            mode: mode_of(&meta),
        });
    }

    // Каталоги сверху, дальше по имени — так же, как в файловом менеджере
    // сборок: список, отсортированный файловой системой, выглядит случайным.
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));

    Ok(DirListing {
        path: rel.to_string(),
        entries,
    })
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.mode())
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}
