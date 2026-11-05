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
