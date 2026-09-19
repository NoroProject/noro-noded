// Файл превышает 150 строк: упаковка и распаковка двух форматов и разбор путей внутри архива рядом.
//! Архивы внутри каталога сервера.
//!
//! Имя внутри архива — такие же чужие данные, как и путь из запроса: запись
//! `../../etc/cron.d/x` распаковывается поверх хоста, если её просто склеить с
//! корнем. Поэтому каждая запись проходит через тот же `fs::resolve`, что и
//! всё остальное, а служебный `.noro/` не читается и не перезаписывается.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// `.zip` или `.tar.gz` — по расширению имени.
fn is_zip(name: &str) -> bool {
    name.to_lowercase().ends_with(".zip")
}

/// Куда ляжет запись архива. `None` — запись пропускается.
fn entry_target(root: &Path, dest_dir: &str, name: &str) -> Option<PathBuf> {
    let rel = if dest_dir.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", dest_dir.trim_end_matches('/'), name)
    };
    if super::is_hidden(&rel) {
        return None;
    }
    super::resolve(root, &rel).ok()
}

/// Распаковать архив в каталог сервера.
///
/// Возвращает, сколько записей легло. Пропущенные — не ошибка: архив с одним
/// дурным именем не повод отказать в остальных трёхстах файлах, но и класть
/// такую запись нельзя.
pub fn unpack(root: &Path, path: &str, dest_dir: &str) -> Result<u64> {
    let archive = super::resolve_existing(root, path)?;
    if is_zip(path) {
        unpack_zip(root, &archive, dest_dir)
    } else {
        unpack_tar(root, &archive, dest_dir)
    }
}

fn unpack_zip(root: &Path, archive: &Path, dest_dir: &str) -> Result<u64> {
    let file = File::open(archive).context("не открыть архив")?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).context("не разобрать zip")?;
    let mut written = 0u64;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        // `enclosed_name` уже отсекает `..` и абсолютные пути, но одного его
        // мало: симлинк внутри каталога сервера он не видит, а `resolve` видит.
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let name = name.to_string_lossy().replace('\\', "/");

        let Some(target) = entry_target(root, dest_dir, &name) else {
            continue;
        };

        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = BufWriter::new(File::create(&target)?);
        std::io::copy(&mut entry, &mut out)?;
        written += 1;
    }

    Ok(written)
}

fn unpack_tar(root: &Path, archive: &Path, dest_dir: &str) -> Result<u64> {
    let file = File::open(archive).context("не открыть архив")?;
    let decoder = flate2::read::GzDecoder::new(BufReader::new(file));
    let mut tar = tar::Archive::new(decoder);
    let mut written = 0u64;

    for entry in tar.entries().context("не разобрать tar")? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().replace('\\', "/");

        // Симлинки и hardlink'и из архива не распаковываются вовсе: их цель
        // указывает куда угодно, и проверять её пришлось бы отдельно от пути.
        if !entry.header().entry_type().is_file() && !entry.header().entry_type().is_dir() {
            continue;
        }

        let Some(target) = entry_target(root, dest_dir, &name) else {
            continue;
        };

        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = BufWriter::new(File::create(&target)?);
        std::io::copy(&mut entry, &mut out)?;
        written += 1;
    }

    Ok(written)
}

/// Упаковать пути сервера в zip внутри его же каталога.
pub fn pack(root: &Path, paths: &[String], dest: &str) -> Result<u64> {
    if super::is_hidden(dest) {
        bail!("служебный каталог не трогается");
    }
    let archive = super::resolve(root, dest)?;
    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut zip = zip::ZipWriter::new(BufWriter::new(File::create(&archive)?));
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut count = 0u64;

    for rel in paths {
        if super::is_hidden(rel) {
            continue;
        }
        let source = super::resolve_existing(root, rel)?;
        if source.is_dir() {
            for entry in walkdir::WalkDir::new(&source)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                let inner = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                zip.start_file(inner, options)?;
                let mut file = File::open(entry.path())?;
                std::io::copy(&mut file, &mut zip)?;
                count += 1;
            }
        } else {
            zip.start_file(rel.replace('\\', "/"), options)?;
            let mut file = File::open(&source)?;
            std::io::copy(&mut file, &mut zip)?;
            count += 1;
        }
    }

    zip.finish()?;
    Ok(count)
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
