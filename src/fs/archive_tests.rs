//! Проверки распаковки: главное здесь — куда записи НЕ ложатся.

use super::*;
use std::fs::File;
use std::io::Write;

struct Sandbox {
    root: PathBuf,
    outside: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("noded-arch-{}", uuid::Uuid::new_v4()));
        let root = base.join("server");
        std::fs::create_dir_all(root.join(".noro")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        Self {
            root,
            outside: base.join("outside"),
        }
    }

    /// Zip с произвольными именами записей — ровно так его и подсунет чужой.
    fn write_zip(&self, name: &str, entries: &[(&str, &str)]) {
        let file = File::create(self.root.join(name)).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (path, body) in entries {
            zip.start_file(*path, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

#[test]
fn a_plain_archive_unpacks_where_it_was_told() {
    let s = Sandbox::new();
    s.write_zip(
        "pack.zip",
        &[("mods/jei.jar", "a"), ("config/jei.cfg", "b")],
    );

    let count = unpack(&s.root, "pack.zip", "").unwrap();

    assert_eq!(count, 2);
    assert!(s.root.join("mods/jei.jar").exists());
    assert!(s.root.join("config/jei.cfg").exists());
}

#[test]
fn a_destination_directory_is_respected() {
    let s = Sandbox::new();
    s.write_zip("pack.zip", &[("jei.jar", "a")]);

    unpack(&s.root, "pack.zip", "mods").unwrap();

    assert!(s.root.join("mods/jei.jar").exists());
}

/// Главная проверка файла: запись с `..` в имени не выходит за каталог сервера.
#[test]
fn an_entry_climbing_out_is_dropped() {
    let s = Sandbox::new();
    s.write_zip(
        "evil.zip",
        &[("../outside/pwned.txt", "x"), ("mods/ok.jar", "a")],
    );

    let count = unpack(&s.root, "evil.zip", "").unwrap();

    assert!(
        !s.outside.join("pwned.txt").exists(),
        "запись ушла за пределы сервера"
    );
    assert!(s.root.join("mods/ok.jar").exists(), "нормальная запись");
    assert_eq!(count, 1, "легла только одна запись");
}

/// В `.noro/` лежит секрет агента: перезаписать его архивом — это подменить
/// личность игрового сервера.
#[test]
fn the_service_directory_cannot_be_overwritten() {
    let s = Sandbox::new();
    std::fs::write(s.root.join(".noro/agent-secret"), "real").unwrap();
    s.write_zip("evil.zip", &[(".noro/agent-secret", "fake")]);

    unpack(&s.root, "evil.zip", "").unwrap();

    let kept = std::fs::read_to_string(s.root.join(".noro/agent-secret")).unwrap();
    assert_eq!(kept, "real");
}

/// То же самое, но подходом через каталог назначения: `dest_dir` тоже приходит
/// из запроса и тоже не должен вести в служебный каталог.
#[test]
fn a_destination_inside_the_service_directory_is_refused() {
    let s = Sandbox::new();
    s.write_zip("pack.zip", &[("x.txt", "a")]);

    unpack(&s.root, "pack.zip", ".noro").unwrap();

    assert!(!s.root.join(".noro/x.txt").exists());
}

#[test]
fn packing_gathers_files_and_skips_the_service_directory() {
    let s = Sandbox::new();
    std::fs::create_dir_all(s.root.join("mods")).unwrap();
    std::fs::write(s.root.join("mods/a.jar"), "a").unwrap();
    std::fs::write(s.root.join(".noro/agent-secret"), "secret").unwrap();

    let count = pack(
        &s.root,
        &["mods".to_string(), ".noro".to_string()],
        "backup.zip",
    )
    .unwrap();

    assert_eq!(count, 1, "только mods/a.jar");
    assert!(s.root.join("backup.zip").exists());
}

/// Распаковка не выходит за остаток квоты, и недописанная запись убирается.
#[test]
fn unpacking_stops_at_the_budget() {
    let s = Sandbox::new();
    let big = "x".repeat(4096);
    s.write_zip("bomb.zip", &[("a.bin", "small"), ("b.bin", big.as_str())]);

    assert!(unpack_within(&s.root, "bomb.zip", "out", Some(1024)).is_err());
    assert!(s.root.join("out/a.bin").exists());
    assert!(!s.root.join("out/b.bin").exists(), "недописанное убрано");

    assert_eq!(
        unpack_within(&s.root, "bomb.zip", "ok", Some(8192)).unwrap(),
        2
    );
}

/// Архив внутри упаковываемого каталога не упаковывает сам себя.
#[test]
fn an_archive_does_not_swallow_itself() {
    let s = Sandbox::new();
    std::fs::create_dir_all(s.root.join("world")).unwrap();
    std::fs::write(s.root.join("world/level.dat"), "lvl").unwrap();

    assert_eq!(
        pack(&s.root, &["world".into()], "world/world.zip").unwrap(),
        1
    );
    let file = File::open(s.root.join("world/world.zip")).unwrap();
    let zip = zip::ZipArchive::new(file).unwrap();
    assert_eq!(zip.len(), 1);
    assert_eq!(zip.name_for_index(0), Some("world/level.dat"));
}
