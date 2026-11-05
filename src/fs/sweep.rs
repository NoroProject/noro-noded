//! Уборка временных файлов, оставшихся от оборванных операций.
//!
//! Запись, закачка и заливка идут через временный файл рядом с целью, и
//! рестарт демона посреди них оставлял его лежать: недокачанный мод на сотни
//! мегабайт, который никто уже не допишет и не удалит, а квота сервера его
//! считает. При старте демон проходит по каталогам серверов и убирает такие
//! файлы — только свои по имени и только старые, чтобы не задеть временный
//! файл операции, начатой уже после старта.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Моложе этого временный файл может принадлежать идущей операции.
pub const MIN_AGE: Duration = Duration::from_secs(3600);

/// Имя временного файла самого демона.
///
/// Нынешние — `.<имя>.noro-<32 hex>`, `.<имя>.noro-part-<32 hex>` и
/// `.<имя>.noro-upload-<32 hex>`; прежние версии оставляли `<имя>.noro-part`
/// и `<имя>.noro-upload`.
pub fn is_leftover(name: &str) -> bool {
    if name.ends_with(".noro-part") || name.ends_with(".noro-upload") {
        return true;
    }
    if !name.starts_with('.') {
        return false;
    }
    let Some((head, suffix)) = name.rsplit_once(".noro-") else {
        return false;
    };
    let token = suffix
        .strip_prefix("part-")
        .or_else(|| suffix.strip_prefix("upload-"))
        .unwrap_or(suffix);
    head.len() > 1 && token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Убрать старые временные файлы в каталоге сервера. Возвращает, сколько убрано.
pub fn sweep(root: &Path, min_age: Duration) -> u64 {
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if !is_leftover(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let old_enough = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= min_age);
        if old_enough && super::nofollow::remove(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_temporary_names_match() {
        assert!(is_leftover(
            ".mod.jar.noro-0123456789abcdef0123456789abcdef"
        ));
        assert!(is_leftover(
            ".mod.jar.noro-part-0123456789abcdef0123456789abcdef"
        ));
        assert!(is_leftover(
            ".world.zip.noro-upload-0123456789abcdef0123456789abcdef"
        ));
        assert!(is_leftover("paper.noro-part"));
        assert!(is_leftover("world.noro-upload"));

        assert!(!is_leftover("mod.jar"));
        assert!(!is_leftover(".noro-pack-files.json"));
        assert!(!is_leftover(".mod.jar.noro-short"));
        assert!(!is_leftover("mod.noro-0123456789abcdef0123456789abcdef"));
        assert!(!is_leftover(
            ".mod.jar.noro-part-zzzz456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn fresh_files_stay_and_old_ones_go() {
        let root = std::env::temp_dir().join(format!("noded-sweep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("mods")).unwrap();
        let tmp = root.join("mods/.a.jar.noro-part-0123456789abcdef0123456789abcdef");
        std::fs::write(&tmp, b"half").unwrap();
        std::fs::write(root.join("mods/a.jar"), b"whole").unwrap();

        assert_eq!(
            sweep(&root, MIN_AGE),
            0,
            "свежий файл может быть чужой операцией"
        );
        assert!(tmp.exists());
        assert_eq!(sweep(&root, Duration::ZERO), 1);
        assert!(!tmp.exists());
        assert!(root.join("mods/a.jar").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
