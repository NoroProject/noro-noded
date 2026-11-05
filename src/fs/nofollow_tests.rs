use super::*;

fn scratch() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("noro-nofollow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn write_replaces_a_link_instead_of_following_it() {
    let root = scratch();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, "host file").unwrap();
    let server = root.join("server");
    std::fs::create_dir_all(&server).unwrap();
    std::os::unix::fs::symlink(&outside, server.join("server.properties")).unwrap();

    write(&server.join("server.properties"), b"motd=hi\n").unwrap();

    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "host file");
    let meta = std::fs::symlink_metadata(server.join("server.properties")).unwrap();
    assert!(meta.file_type().is_file(), "the link itself was replaced");
    assert_eq!(
        std::fs::read_to_string(server.join("server.properties")).unwrap(),
        "motd=hi\n"
    );
}

#[test]
fn write_refuses_a_linked_directory() {
    let root = scratch();
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let server = root.join("server");
    std::fs::create_dir_all(&server).unwrap();
    std::os::unix::fs::symlink(&outside, server.join(".noro")).unwrap();

    assert!(write(&server.join(".noro/limits.json"), b"{}").is_err());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn write_leaves_no_temporary_files() {
    let root = scratch();
    write(&root.join("a.txt"), b"1").unwrap();
    write(&root.join("a.txt"), b"2").unwrap();
    let names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("a.txt")]);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "2");
}

#[test]
fn open_refuses_links() {
    let root = scratch();
    let outside = root.join("secret");
    std::fs::write(&outside, "host secret").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("world.zip")).unwrap();

    assert!(open_existing(&root.join("world.zip"), false).is_err());
    assert!(open_existing(&root.join("world.zip"), true).is_err());
    std::fs::write(root.join("plain"), "ok").unwrap();
    assert!(open_existing(&root.join("plain"), false).is_ok());
}

#[test]
fn remove_takes_the_link_and_leaves_the_target() {
    let root = scratch();
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "host").unwrap();
    let server = root.join("server");
    std::fs::create_dir_all(&server).unwrap();
    std::os::unix::fs::symlink(&outside, server.join("link")).unwrap();

    remove(&server.join("link")).unwrap();
    assert!(std::fs::symlink_metadata(server.join("link")).is_err());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "host"
    );
}

#[test]
fn rename_moves_entries_inside_checked_directories() {
    let root = scratch();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/x"), "1").unwrap();
    rename(&root.join("a/x"), &root.join("y")).unwrap();
    assert_eq!(std::fs::read_to_string(root.join("y")).unwrap(), "1");
}

#[test]
fn create_new_does_not_walk_through_a_planted_link() {
    let root = scratch();
    let outside = root.join("outside.txt");
    std::os::unix::fs::symlink(&outside, root.join("upload.bin")).unwrap();
    assert!(create_new(&root.join("upload.bin")).is_err());
    assert!(!outside.exists());
}

/// Подменить можно любой каталог на пути, не только ближайший. Сверка
/// дескриптора есть только в Linux — там это и проверяется.
#[test]
#[cfg(target_os = "linux")]
fn a_link_higher_up_the_path_is_refused() {
    let root = scratch();
    let outside = root.join("outside");
    std::fs::create_dir_all(outside.join("plugins")).unwrap();
    std::fs::write(outside.join("plugins/secret.txt"), "host").unwrap();
    let server = root.join("server");
    std::fs::create_dir_all(&server).unwrap();
    std::os::unix::fs::symlink(&outside, server.join("data")).unwrap();

    assert!(write(&server.join("data/plugins/cron"), b"x").is_err());
    assert!(!outside.join("plugins/cron").exists());
    assert!(open_existing(&server.join("data/plugins/secret.txt"), false).is_err());
    assert!(remove(&server.join("data/plugins/secret.txt")).is_err());
    assert!(outside.join("plugins/secret.txt").exists());
}
