// Файл превышает 150 строк: тишина сервера на время упаковки, сама упаковка и её проверки — одна операция бэкапа.
//! Упаковка сервера: полным архивом или снимком в хранилище кусков.
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
pub(super) const NEVER: &[&str] = &["logs", "crash-reports", ".noro", "backups"];

#[derive(Debug)]
pub struct BackupResult {
    /// Сколько байт бэкап добавил на диск: архив целиком или новые куски.
    pub bytes: u64,
    pub sha256: String,
    /// Размер файлов в снимке; у архива неизвестен.
    pub source_bytes: Option<u64>,
}

/// Куда упаковывать.
pub enum Target {
    /// Полный `.tar.gz` по этому пути.
    Archive(PathBuf),
    /// Снимок в хранилище кусков сервера.
    Snapshot { repo: PathBuf, backup: uuid::Uuid },
}

/// Архив перерос отведённое ему место — лимит бэкапов сервера или запас диска
/// ноды. Отдельным типом, чтобы отказ дошёл до мастера своим кодом, а не
/// «докер не смог».
#[derive(Debug)]
pub struct OverBudget {
    pub limit: u64,
}

impl std::fmt::Display for OverBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "архив не помещается в отведённые ему {} МБ",
            self.limit / 1024 / 1024
        )
    }
}

impl std::error::Error for OverBudget {}

/// Снять бэкап. `limit` — сколько байт архиву можно занять; упаковка
/// обрывается, как только он вырастает больше, а не после того, как съела
/// весь диск.
pub async fn create(
    registry: &Registry,
    root: &Path,
    target: Target,
    ignore: &[String],
    limit: Option<u64>,
) -> Result<BackupResult> {
    let quiesced = quiesce(registry, root).await;

    // Упаковка мира — это сжатие гигабайтов: на рабочем потоке рантайма
    // она останавливает консоли всех серверов ноды на всё время бэкапа.
    let (src, ignore) = (root.to_path_buf(), ignore.to_vec());
    let result = tokio::task::spawn_blocking(move || match target {
        Target::Archive(dst) => pack(&src, &dst, &ignore, limit),
        Target::Snapshot { repo, backup } => {
            let repo = super::repo::Repo::open(&repo)?;
            super::snapshot::take(&src, &repo, backup, &ignore, limit)
        }
    })
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

fn pack(root: &Path, dest: &Path, ignore: &[String], limit: Option<u64>) -> Result<BackupResult> {
    // Каталог бэкапов вне папки сервера, но создаётся архив всё равно мимо
    // ссылок: так он не зависит от того, кто и что положил на его место.
    let file = crate::fs::nofollow::create_replacing(dest).context("не создать файл архива")?;
    let over = Arc::new(AtomicBool::new(false));
    let result = write_archive(root, file, ignore, limit, over.clone()).map_err(|e| match limit {
        Some(limit) if over.load(Ordering::Relaxed) => anyhow::Error::new(OverBudget { limit }),
        _ => e,
    });
    if result.is_err() {
        // Недописанный архив выглядит как бэкап, а восстановить из него можно
        // только половину мира.
        let _ = crate::fs::nofollow::remove(dest);
    }
    result
}

/// Полный архив без тишины сервера — для замеров в тестах снимков.
#[cfg(test)]
pub(super) fn pack_for_test(root: &Path, dest: &Path) -> u64 {
    pack(root, dest, &[], None).unwrap().bytes
}

fn write_archive(
    root: &Path,
    file: std::fs::File,
    ignore: &[String],
    limit: Option<u64>,
    over: Arc<AtomicBool>,
) -> Result<BackupResult> {
    // Хеш и размер считаются по ходу записи: перечитывать готовый архив ради
    // них — это ещё раз весь мир с диска.
    let sink = Counting {
        inner: std::io::BufWriter::new(file),
        hasher: sha2::Sha256::new(),
        bytes: 0,
        limit,
        over,
    };
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
        source_bytes: None,
    })
}

/// Запись, которая попутно считает байты и sha256 и обрывается на пределе.
struct Counting<W> {
    inner: W,
    hasher: sha2::Sha256,
    bytes: u64,
    limit: Option<u64>,
    /// Поднимается, когда запись упёрлась в предел: ошибка ввода-вывода сама
    /// по себе не отличает «кончилось отведённое» от «сломался диск».
    over: Arc<AtomicBool>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self
            .limit
            .is_some_and(|limit| self.bytes.saturating_add(buf.len() as u64) > limit)
        {
            self.over.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other("архив перерос отведённое место"));
        }
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn skipped(rel: &str, ignore: &[String]) -> bool {
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

        let r = pack(&root, &dest, &[], None).unwrap();
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

        pack(&root, &dest, &[], None).unwrap();
        let bytes = std::fs::read(&dest).unwrap();
        let count = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]))
            .entries()
            .unwrap()
            .count();
        assert_eq!(count, 0);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Архив, переросший отведённое место, не остаётся лежать огрызком, и
    /// отказ узнаётся по типу, а не по тексту.
    #[test]
    fn an_archive_over_its_budget_is_cut_and_removed() {
        let base = std::env::temp_dir().join(format!("noded-backup-{}", uuid::Uuid::new_v4()));
        let root = base.join("server");
        std::fs::create_dir_all(root.join("world")).unwrap();
        // Случайные байты не сжимаются: архив гарантированно больше предела.
        let mut noise = vec![0u8; 256 * 1024];
        rand::Rng::fill(&mut rand::thread_rng(), &mut noise[..]);
        std::fs::write(root.join("world/region.mca"), noise).unwrap();
        let dest = base.join("b.tar.gz");

        let err = pack(&root, &dest, &[], Some(16 * 1024)).unwrap_err();
        assert!(err.downcast_ref::<OverBudget>().is_some(), "{err:#}");
        assert!(!dest.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn extra_patterns_are_honoured() {
        let ignore = vec!["mods/**".to_string()];
        assert!(skipped("mods/jei.jar", &ignore));
        assert!(!skipped("world/level.dat", &ignore));
    }
}
