//! Сборка мусора в хранилище кусков: пометить всё, на что ссылаются
//! манифесты, и убрать остальное.
//!
//! Счётчиков ссылок нет намеренно. Счётчик — ещё одно состояние, которое
//! сбой посреди удаления оставляет неверным, и тогда кусок живого снимка
//! уходит, а мир восстанавливается дырявым. Пометка же каждый раз заново
//! выводит ответ из самих манифестов, и сбой в любой момент оставляет в худшем
//! случае лишние куски — их уберёт следующая сборка.
//!
//! Идёт только тогда, когда в хранилище никто не пишет: бэкапы, удаление и
//! восстановление одного сервера демон не пускает одновременно (`server::busy`).

use anyhow::{Context, Result};
use std::collections::HashSet;

use super::manifest::ChunkId;
use super::repo::Repo;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub removed: usize,
    pub freed: u64,
}

/// Убрать куски, на которые не ссылается ни один снимок, и временные файлы
/// оборванных записей.
///
/// Нечитаемый манифест останавливает сборку целиком: не зная, на что он
/// ссылается, нельзя сказать, какой кусок ничей.
pub fn collect(repo: &Repo) -> Result<Collected> {
    let mut live: HashSet<ChunkId> = HashSet::new();
    for backup in repo.snapshots()? {
        let manifest = repo
            .manifest(backup)
            .context("снимок не прочитан — сборка мусора отменена")?;
        live.extend(manifest.chunk_ids());
    }

    let mut done = Collected::default();
    for dir in repo.chunk_dirs()? {
        for entry in dir.read_dir()? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let dead = match ChunkId::parse(name) {
                Some(id) => !live.contains(&id),
                // Временный файл: писать сейчас некому, значит он брошен.
                None => crate::fs::sweep::is_leftover(name),
            };
            if !dead {
                continue;
            }
            let path = dir.entry(name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                done.removed += 1;
                done.freed += meta.len();
            }
        }
    }

    if let Some(dir) = repo.snapshots_dir() {
        for entry in dir.read_dir()?.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(crate::fs::sweep::is_leftover) {
                let _ = std::fs::remove_file(dir.entry(&name));
            }
        }
    }
    Ok(done)
}
