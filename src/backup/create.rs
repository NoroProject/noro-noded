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

    let file = std::fs::File::create(dest).context("не создать файл архива")?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(encoder);

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() || skipped(&rel_str, ignore) {
            continue;
        }

        if entry.file_type().is_file() {
            tar.append_path_with_name(entry.path(), rel)?;
        }
    }

    tar.finish()?;
    drop(tar);

    let bytes = std::fs::metadata(dest)?.len();
    Ok(BackupResult {
        sha256: sha256_of(dest)?,
        bytes,
    })
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

fn sha256_of(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
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

    #[test]
    fn extra_patterns_are_honoured() {
        let ignore = vec!["mods/**".to_string()];
        assert!(skipped("mods/jei.jar", &ignore));
        assert!(!skipped("world/level.dat", &ignore));
    }
}
