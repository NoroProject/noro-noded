//! Точка входа сервера и правка `user_jvm_args.txt`.

use super::*;

struct Dir(std::path::PathBuf);

impl Dir {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("noded-entry-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn file(&self, rel: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// То, что на диске есть, не перепроверяется: карточка права.
#[test]
fn an_existing_entry_point_is_kept() {
    let d = Dir::new();
    d.file("paper-1.21.1-133.jar");
    assert_eq!(
        resolve_entry_point(&d.0, "paper-1.21.1-133.jar"),
        "paper-1.21.1-133.jar"
    );
}

/// Главная проверка файла. У сервера, поставленного до того, как мастер
/// научился запоминать точку входа, в карточке стоит `server.jar` — и
/// пересборка контейнера по ней ломала работающий сервер.
#[test]
fn a_stale_card_is_corrected_from_the_disk() {
    let d = Dir::new();
    d.file("paper-1.21.1-133.jar");
    assert_eq!(
        resolve_entry_point(&d.0, "server.jar"),
        "paper-1.21.1-133.jar"
    );
}

/// У NeoForge в корне вообще нет jar: запускается он файлом аргументов,
/// который написал установщик.
#[test]
fn a_neoforge_args_file_is_found_instead_of_a_jar() {
    let d = Dir::new();
    d.file("libraries/net/neoforged/neoforge/21.1.77/unix_args.txt");
    assert_eq!(
        resolve_entry_point(&d.0, "server.jar"),
        "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt"
    );
}

/// Файл аргументов на месте — его и оставляем, не уходя искать заново.
#[test]
fn an_existing_args_file_is_kept() {
    let d = Dir::new();
    d.file("libraries/net/neoforged/neoforge/21.1.77/unix_args.txt");
    let wanted = "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt";
    assert_eq!(resolve_entry_point(&d.0, wanted), wanted);
}

/// Установщик остаётся лежать в корне рядом с результатом. Запустить его
/// вместо сервера — это установка по кругу на каждый старт.
#[test]
fn the_installer_is_never_the_entry_point() {
    let d = Dir::new();
    d.file("forge-1.16.5-36.2.39-installer.jar");
    d.file("forge-1.16.5-36.2.39.jar");
    assert_eq!(
        resolve_entry_point(&d.0, "server.jar"),
        "forge-1.16.5-36.2.39.jar"
    );
}

/// Ничего похожего на диске нет — присланное остаётся как есть: отказ при
/// старте с внятной строкой честнее подставленного наугад файла.
#[test]
fn nothing_on_disk_leaves_the_card_alone() {
    let d = Dir::new();
    assert_eq!(resolve_entry_point(&d.0, "server.jar"), "server.jar");
}

#[test]
fn the_core_name_comes_from_the_url() {
    assert_eq!(
        core_file_name("https://fill-data.papermc.io/v1/objects/39bd8c00/paper-1.21.1-133.jar"),
        "paper-1.21.1-133.jar"
    );
    // Ссылка из нашего стора — content-addressed, имени файла в ней нет.
    assert_eq!(
        core_file_name("https://api.example.com/files/abc123"),
        "server.jar"
    );
}

#[test]
fn forge_universal_wins_over_minecraft_server() {
    let d = Dir::new();
    d.file("minecraft_server.1.7.10.jar");
    d.file("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar");
    d.file("forge-1.7.10-10.13.4.1614-1.7.10-installer.jar");
    assert_eq!(
        resolve_entry_point(&d.0, "server.jar"),
        "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"
    );
}

#[test]
fn lwjgl3ify_wins_over_forge_universal() {
    let d = Dir::new();
    d.file("minecraft_server.1.7.10.jar");
    d.file("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar");
    d.file("lwjgl3ify-forgePatches.jar");
    assert_eq!(
        resolve_entry_point(&d.0, "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"),
        "lwjgl3ify-forgePatches.jar"
    );
}

#[test]
fn unix_args_wins_over_minecraft_server() {
    let d = Dir::new();
    d.file("minecraft_server.jar");
    d.file("libraries/net/minecraftforge/forge/1.20.1-47.4.13/unix_args.txt");
    assert_eq!(
        resolve_entry_point(&d.0, "minecraft_server.jar"),
        "@libraries/net/minecraftforge/forge/1.20.1-47.4.13/unix_args.txt"
    );
}

#[test]
fn sync_user_jvm_args_updates_agent() {
    let d = Dir::new();
    let path = d.0.join("user_jvm_args.txt");
    std::fs::write(&path, "# comment\n-Xmx4G\n").unwrap();
    sync_user_jvm_args(&d.0, Some("https://example.com/api/yggdrasil")).unwrap();
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("-javaagent:"));
    assert!(updated.contains("https://example.com/api/yggdrasil"));

    // Удаление при None
    sync_user_jvm_args(&d.0, None).unwrap();
    let removed = std::fs::read_to_string(&path).unwrap();
    assert!(!removed.contains("-javaagent:"));
    assert!(removed.contains("-Xmx4G"));
}
