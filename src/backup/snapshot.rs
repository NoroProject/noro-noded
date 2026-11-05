// Файл превышает 150 строк: обход папки сервера, нарезка файлов и откат оборванного снимка — одна операция.
//! Снимок сервера в хранилище кусков.
//!
//! Файл режется на куски по содержимому (FastCDC): граница ставится там, где
//! это велят сами байты, а не через каждые N байт. Правка в середине региона
//! меняет один-два куска вокруг себя, а не сдвигает все следующие, как при
//! нарезке поровну, — поэтому мир, поменявшийся на проценты, и добавляет в
//! хранилище проценты.
//!
//! Файл, у которого с прошлого снимка не сдвинулись ни размер, ни `ctime`, не
//! читается вовсе: его куски берутся из прошлого манифеста. Мир на двадцать
//! гигабайт иначе перечитывался бы целиком ради каждого бэкапа.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use uuid::Uuid;

use super::create::{skipped, BackupResult, OverBudget, NEVER};
use super::manifest::{Chunk, ChunkId, Entry, Manifest, Stamp, VERSION};
use super::repo::{OutOfRoom, Repo};

/// Границы куска. Нижняя не даёт мелочи раздуть число файлов хранилища,
/// верхняя держит кусок в памяти маленьким; средний мегабайт — это несколько
/// кусков на регион в пару мегабайт.
pub const MIN_CHUNK: u32 = 256 * 1024;
pub const AVG_CHUNK: u32 = 1024 * 1024;
pub const MAX_CHUNK: u32 = 4 * 1024 * 1024;

/// Снять снимок `root` в хранилище. `limit` — сколько новых байт можно
/// записать: кусков, которых в хранилище ещё не было, и манифеста.
pub fn take(
    root: &Path,
    repo: &Repo,
    backup: Uuid,
    ignore: &[String],
    limit: Option<u64>,
) -> Result<BackupResult> {
    // Прошлая упаковка оборвалась вместе с демоном: её куски ничьи и
    // занимают место, которое сейчас посчитают в лимит.
    if repo.interrupted() {
        super::gc::collect(repo)?;
    }
    repo.begin(backup)?;
    let result = pack(root, repo, backup, ignore, limit);
    if result.is_err() {
        // Куски несостоявшегося снимка не нужны никому. Если и сборка не
        // пройдёт, отметка останется, и их уберёт следующая упаковка.
        if super::gc::collect(repo).is_err() {
            return result;
        }
    }
    repo.end();
    result.map_err(|e| match (e.downcast_ref::<OutOfRoom>(), limit) {
        (Some(_), Some(limit)) => anyhow::Error::new(OverBudget { limit }),
        _ => e,
    })
}

fn pack(
    root: &Path,
    repo: &Repo,
    backup: Uuid,
    ignore: &[String],
    limit: Option<u64>,
) -> Result<BackupResult> {
    let parent = repo.latest();
    let known: HashMap<&str, &Entry> = parent
        .iter()
        .flat_map(|m| m.entries.iter())
        .filter(|e| !e.dir)
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut added = 0u64;
    let mut entries = Vec::new();
    let walk = walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        // Логи и служебный каталог не обходятся вовсе: в логах бывают
        // десятки тысяч файлов, и ни один в бэкап не идёт.
        .filter_entry(|e| e.depth() != 1 || !NEVER.contains(&&*e.file_name().to_string_lossy()));
    for entry in walk.filter_map(|e| e.ok()) {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || skipped(&rel, ignore) {
            continue;
        }
        let kind = entry.file_type();
        if kind.is_dir() {
            if let Ok(meta) = entry.metadata() {
                entries.push(Entry {
                    path: rel,
                    dir: true,
                    mode: mode_of(&meta),
                    mtime: mtime_of(&meta),
                    size: 0,
                    chunks: vec![],
                    stamp: None,
                });
            }
            continue;
        }
        // Ссылки, сокеты и прочее не попадают в снимок: формат умеет только
        // файлы и каталоги, и восстановление ничего другого не создаст.
        if !kind.is_file() {
            continue;
        }

        // Открытие мимо ссылок и в сверенном каталоге — как у полного архива:
        // ссылка, подложенная на место файла посреди обхода, иначе унесла бы
        // в снимок файл хоста.
        let file = match crate::fs::nofollow::open_existing(entry.path(), false) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %rel, error = %e, "файл не попал в снимок");
                continue;
            }
        };
        let meta = file.metadata()?;
        let stamp = stamp_of(&meta);
        let reused = known
            .get(rel.as_str())
            .filter(|old| old.size == meta.len() && old.stamp == Some(stamp))
            .filter(|old| old.chunks.iter().all(|c| repo.has_chunk(&c.id)));
        let (size, chunks) = match reused {
            Some(old) => (old.size, old.chunks.clone()),
            None => {
                let room = limit.map(|l| l.saturating_sub(added));
                let cut = cut(repo, file, meta.len(), room)
                    .with_context(|| format!("файл {rel} не попал в снимок"))?;
                added += cut.added;
                (cut.size, cut.chunks)
            }
        };
        entries.push(Entry {
            path: rel,
            dir: false,
            mode: mode_of(&meta),
            mtime: mtime_of(&meta),
            size,
            chunks,
            stamp: Some(stamp),
        });
    }

    let manifest = Manifest {
        version: VERSION,
        backup,
        created: chrono::Utc::now().timestamp(),
        entries,
    };
    let (len, sha256) = repo.commit(&manifest)?;
    if limit.is_some_and(|l| added + len > l) {
        // Манифест уже лёг: такой снимок держит больше, чем ему отведено, и
        // убирается так же, как недописанный.
        repo.remove_snapshot(backup)?;
        anyhow::bail!(OutOfRoom);
    }
    Ok(BackupResult {
        bytes: added + len,
        sha256,
        source_bytes: Some(manifest.source_bytes()),
    })
}

pub(super) struct Cut {
    pub size: u64,
    pub chunks: Vec<Chunk>,
    pub added: u64,
}

/// Нарезать файл и положить новые куски. Читается ровно `len` байт: файл
/// живого сервера может расти посреди чтения, а размер в манифесте обязан
/// совпасть с тем, что в куски легло.
pub(super) fn cut(repo: &Repo, file: impl Read, len: u64, room: Option<u64>) -> Result<Cut> {
    let mut out = Cut {
        size: 0,
        chunks: vec![],
        added: 0,
    };
    let chunker = fastcdc::v2020::StreamCDC::new(file.take(len), MIN_CHUNK, AVG_CHUNK, MAX_CHUNK);
    for piece in chunker {
        let piece = piece.map_err(|e| anyhow::anyhow!("чтение файла: {e}"))?;
        let id = ChunkId::of(&piece.data);
        let room = room.map(|r| r.saturating_sub(out.added));
        out.added += repo.put_chunk(&id, &piece.data, room)?;
        out.size += piece.length as u64;
        out.chunks.push(Chunk {
            id,
            len: piece.length as u32,
        });
    }
    Ok(out)
}

#[cfg(unix)]
fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    Stamp {
        dev: 0,
        ino: 0,
        mtime_ns: mtime_of(meta) * 1_000_000_000,
        ctime_ns: 0,
    }
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else {
        0o644
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(all(test, unix))]
#[path = "snapshot_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "snapshot_figures.rs"]
mod figures;
