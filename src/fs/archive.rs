// Файл превышает 150 строк: упаковка и распаковка двух форматов и разбор путей внутри архива рядом.
//! Архивы внутри каталога сервера.
//!
//! Имя внутри архива — такие же чужие данные, как и путь из запроса: запись
//! `../../etc/cron.d/x` распаковывается поверх хоста, если её просто склеить с
//! корнем. Поэтому каждая запись проходит через тот же `fs::resolve`, что и
//! всё остальное, а служебный `.noro/` не читается и не перезаписывается.

use anyhow::{bail, Context, Result};
use std::io::{BufReader, BufWriter, Read, Write};
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
    unpack_within(root, path, dest_dir, None)
}

/// То же, но не больше `budget` байт распакованного.
///
/// Размер внутри архива ничем не ограничен: zip в мегабайт разворачивается в
/// десятки гигабайт, и распаковка обходила квоту сервера и забивала диск
/// всей ноде. Потолок — остаток квоты; запись, на которой он кончился,
/// убирается, и распаковка останавливается.
pub fn unpack_within(root: &Path, path: &str, dest_dir: &str, budget: Option<u64>) -> Result<u64> {
    let archive = super::resolve_existing(root, path)?;
    let mut sink = Sink { budget, spent: 0 };
    if is_zip(path) {
        unpack_zip(root, &archive, dest_dir, &mut sink)
    } else {
        unpack_tar(root, &archive, dest_dir, &mut sink)
    }
}

/// Сколько уже распаковано и сколько ещё можно.
struct Sink {
    budget: Option<u64>,
    spent: u64,
}

impl Sink {
    fn write(&mut self, target: &Path, reader: &mut dyn Read) -> Result<()> {
        let mut out = BufWriter::new(super::nofollow::create_replacing(target)?);
        let copied = match self.budget {
            Some(budget) => {
                let room = budget.saturating_sub(self.spent);
                let copied = std::io::copy(&mut reader.take(room.saturating_add(1)), &mut out)?;
                if copied > room {
                    drop(out);
                    let _ = super::nofollow::remove(target);
                    bail!("распакованное не помещается: кончилась квота сервера или место на ноде");
                }
                copied
            }
            None => std::io::copy(reader, &mut out)?,
        };
        out.flush()?;
        self.spent += copied;
        Ok(())
    }
}

fn unpack_zip(root: &Path, archive: &Path, dest_dir: &str, sink: &mut Sink) -> Result<u64> {
    let file = super::nofollow::open_existing(archive, false).context("не открыть архив")?;
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
            super::mkdir::create_dir_all(root, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            super::mkdir::create_dir_all(root, parent)?;
        }
        sink.write(&target, &mut entry)?;
        written += 1;
    }

    Ok(written)
}

fn unpack_tar(root: &Path, archive: &Path, dest_dir: &str, sink: &mut Sink) -> Result<u64> {
    let file = super::nofollow::open_existing(archive, false).context("не открыть архив")?;
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
            super::mkdir::create_dir_all(root, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            super::mkdir::create_dir_all(root, parent)?;
        }
        sink.write(&target, &mut entry)?;
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
    // Имена в архиве — от настоящего корня: пути источников канонические, и
    // от неканонического корня они не отрезались, а целиком уходили в архив
    // путём хоста.
    let root = root.canonicalize()?;
    let root = root.as_path();
    if let Some(parent) = archive.parent() {
        super::mkdir::create_dir_all(root, parent)?;
    }

    let mut zip = zip::ZipWriter::new(BufWriter::new(super::nofollow::create_replacing(&archive)?));
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
                // Архив, который ложится внутрь упаковываемого каталога,
                // попадал в обход сам: читался, пока в него же писался, и рос,
                // пока не кончится диск.
                .filter(|e| e.path() != archive)
            {
                let inner = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                zip.start_file(inner, options)?;
                let mut file = super::nofollow::open_existing(entry.path(), false)?;
                std::io::copy(&mut file, &mut zip)?;
                count += 1;
            }
        } else {
            zip.start_file(rel.replace('\\', "/"), options)?;
            let mut file = super::nofollow::open_existing(&source, false)?;
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
