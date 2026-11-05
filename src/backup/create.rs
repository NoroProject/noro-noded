// Файл превышает 150 строк: тишина сервера на время упаковки, сама упаковка и её проверки — одна операция бэкапа.
//! Упаковка сервера в архив.
//!
//! Живой мир нельзя просто затарить: Minecraft держит регионы открытыми и
//! пишет в них когда захочет, и половина архива окажется от состояния «до», а
//! половина «после» — это битые регионы при восстановлении. Поэтому перед
//! упаковкой сервер просят сохраниться и замолчать, а после — отпускают.

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::Digest;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::server::registry::Registry;

/// Сколько ждать, пока сервер допишет мир после `save-all flush`.
///
/// Флаш не подтверждается ничем машиночитаемым: сервер печатает строку в лог, а
/// её формат менялся между версиями. Пауза грубее, но она работает на всех.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// Что не попадает в архив никогда.
///
/// Логи — потому что их десятки гигабайт и они бесполезны в восстановлении;
/// `.noro` — потому что там секрет агента, и ему незачем уезжать в чужое
/// хранилище вместе с миром.
const NEVER: &[&str] = &["logs", "crash-reports", ".noro", "backups"];

pub struct BackupResult {
    pub bytes: u64,
    pub sha256: String,
}

pub async fn create(
    registry: &Registry,
    root: &Path,
    dest: &Path,
    ignore: &[String],
) -> Result<BackupResult> {
    let quiesced = quiesce(registry, root).await;

    // Упаковка мира — это gzip поверх гигабайтов: на рабочем потоке рантайма
    // она останавливает консоли всех серверов ноды на всё время бэкапа.
    let (src, dst, ignore) = (root.to_path_buf(), dest.to_path_buf(), ignore.to_vec());
    let result = tokio::task::spawn_blocking(move || pack(&src, &dst, &ignore))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("упаковка не выполнена: {e}")));

    if quiesced {
        // Отпускаем сервер даже если упаковка провалилась: иначе он останется
        // молчать, и мир перестанет сохраняться совсем.
        resume(registry, root).await;
    }

    result
}

/// Попросить сервер сохраниться и приостановить запись.
///
/// Возвращает `false`, если сервер не запущен: тогда и отпускать нечего, а
/// файлы на диске и так неподвижны.
async fn quiesce(registry: &Registry, root: &Path) -> bool {
    let Some(server) = server_id_of(root) else {
        return false;
    };
    let Some(handle) = registry.get(server) else {
        return false;
    };
    if !handle.state().power.is_up() {
        return false;
    }

    let mut guard = handle.writer.lock().await;
    let Some(writer) = guard.as_mut() else {
        return false;
    };

    let _ = writer.send("save-off").await;
    let _ = writer.send("save-all flush").await;
    // Замок отпускается здесь, до паузы: держать его пять секунд значит
    // заблокировать на это время и консоль, и команды владельца.
    drop(guard);

    tokio::time::sleep(FLUSH_WAIT).await;
    true
}

async fn resume(registry: &Registry, root: &Path) {
    let Some(server) = server_id_of(root) else {
        return;
    };
    let Some(handle) = registry.get(server) else {
        return;
    };
    let mut writer = handle.writer.lock().await;
    if let Some(writer) = writer.as_mut() {
        let _ = writer.send("save-on").await;
    }
}

/// Идентификатор сервера — из имени каталога: он и есть `servers/<uuid>`.
fn server_id_of(root: &Path) -> Option<uuid::Uuid> {
    root.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| uuid::Uuid::parse_str(n).ok())
}

fn pack(root: &Path, dest: &Path, ignore: &[String]) -> Result<BackupResult> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Каталог бэкапов лежит внутри папки сервера, а её меняет процесс сервера:
    // на месте архива могла оказаться ссылка на файл хоста.
    let file = crate::fs::nofollow::create_replacing(dest).context("не создать файл архива")?;
    let result = write_archive(root, file, ignore);
    if result.is_err() {
        // Недописанный архив выглядит как бэкап, а восстановить из него можно
        // только половину мира.
        let _ = crate::fs::nofollow::remove(dest);
    }
    result
}

fn write_archive(root: &Path, file: std::fs::File, ignore: &[String]) -> Result<BackupResult> {
    // Хеш и размер считаются по ходу записи: перечитывать готовый архив ради
    // них — это ещё раз весь мир с диска.
    let sink = Counting::new(std::io::BufWriter::new(file));
    let mut tar = tar::Builder::new(GzEncoder::new(sink, Compression::default()));

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() || skipped(&rel_str, ignore) || !entry.file_type().is_file() {
            continue;
        }

        // Файл открывается без ссылок и в сверенном каталоге: обход идёт
        // минутами, и ссылка, подложенная на место файла или каталога над ним,
        // иначе уносила в архив файл хоста — например, конфиг ноды с её
        // токеном, — а архив потом скачивает владелец сервера.
        let source = match crate::fs::nofollow::open_existing(entry.path(), false) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "файл не попал в бэкап");
                continue;
            }
        };
        let meta = source.metadata()?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
        // Ровно столько байт, сколько записано в заголовке: файл живого
        // сервера может вырасти или укоротиться посреди чтения, а запись другой
        // длины ломает в tar всё, что идёт за ней.
        let size = meta.len();
        let body = source.take(size).chain(std::io::repeat(0)).take(size);
        tar.append_data(&mut header, rel, body)?;
    }

    let mut sink = tar.into_inner()?.finish()?;
    sink.flush()?;
    sink.inner.get_ref().sync_all()?;
    Ok(BackupResult {
        bytes: sink.bytes,
        sha256: hex::encode(sink.hasher.finalize()),
    })
}

/// Запись, которая попутно считает байты и sha256.
struct Counting<W> {
    inner: W,
    hasher: sha2::Sha256,
    bytes: u64,
}

impl<W> Counting<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            bytes: 0,
        }
    }
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn skipped(rel: &str, ignore: &[String]) -> bool {
    let head = rel.split('/').next().unwrap_or("");
    if NEVER.contains(&head) {
        return true;
    }
    ignore
        .iter()
        .any(|pattern| schema::path_rules::matches(rel, pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_and_the_service_directory_never_get_packed() {
        assert!(skipped("logs/latest.log", &[]));
        assert!(skipped(".noro/agent-secret", &[]));
        assert!(skipped("backups/old.tar.gz", &[]));
        assert!(skipped("crash-reports/crash.txt", &[]));
    }

    #[test]
    fn the_world_and_configs_do() {
        assert!(!skipped("world/level.dat", &[]));
        assert!(!skipped("server.properties", &[]));
        assert!(!skipped("mods/jei.jar", &[]));
    }

    /// Хеш, посчитанный по ходу записи, — хеш того файла, что лёг на диск.
    #[test]
    fn the_archive_hash_matches_the_file() {
        let base = std::env::temp_dir().join(format!("noded-backup-{}", uuid::Uuid::new_v4()));
        let root = base.join("server");
        std::fs::create_dir_all(root.join("world")).unwrap();
        std::fs::write(root.join("world/level.dat"), b"lvl").unwrap();
        std::fs::write(root.join("server.properties"), b"motd=x").unwrap();
        let dest = base.join("b.tar.gz");

        let r = pack(&root, &dest, &[]).unwrap();
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(r.bytes, bytes.len() as u64);
        assert_eq!(r.sha256, hex::encode(sha2::Sha256::digest(&bytes)));

        let mut names: Vec<String> = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]))
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["server.properties", "world/level.dat"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn links_are_not_followed_into_the_archive() {
        let base = std::env::temp_dir().join(format!("noded-backup-{}", uuid::Uuid::new_v4()));
        let root = base.join("server");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(base.join("noded.toml"), b"token=secret").unwrap();
        std::os::unix::fs::symlink(base.join("noded.toml"), root.join("cfg.toml")).unwrap();
        let dest = base.join("b.tar.gz");

        pack(&root, &dest, &[]).unwrap();
        let bytes = std::fs::read(&dest).unwrap();
        let count = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]))
            .entries()
            .unwrap()
            .count();
        assert_eq!(count, 0);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn extra_patterns_are_honoured() {
        let ignore = vec!["mods/**".to_string()];
        assert!(skipped("mods/jei.jar", &ignore));
        assert!(!skipped("world/level.dat", &ignore));
    }
}
