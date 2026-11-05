//! Копирование: что копируется целиком и куда копия лечь не может.

use super::*;
use std::path::PathBuf;

fn sandbox() -> PathBuf {
    let root = std::env::temp_dir().join(format!("noded-io-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("world/region")).unwrap();
    std::fs::create_dir_all(root.join(".noro")).unwrap();
    std::fs::write(root.join("world/level.dat"), b"lvl").unwrap();
    std::fs::write(root.join("world/region/r.0.0.mca"), b"mca").unwrap();
    root
}

#[test]
fn a_directory_is_copied_whole() {
    let root = sandbox();
    assert_eq!(copy(&root, "world", "world-copy").unwrap(), 2);
    assert_eq!(
        std::fs::read(root.join("world-copy/region/r.0.0.mca")).unwrap(),
        b"mca"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_copy_does_not_overwrite_or_nest_into_itself() {
    let root = sandbox();
    std::fs::write(root.join("taken.txt"), b"x").unwrap();
    assert!(copy(&root, "world/level.dat", "taken.txt").is_err());
    assert!(copy(&root, "world", "world/inner").is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_service_directory_is_off_limits() {
    let root = sandbox();
    assert!(copy(&root, ".noro", "noro-copy").is_err());
    assert!(copy(&root, "world", ".noro/world").is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn deleting_a_link_keeps_what_it_points_to() {
    let root = sandbox();
    std::os::unix::fs::symlink(root.join("world"), root.join("old-world")).unwrap();
    let outside = root.with_extension("outside");
    std::fs::write(&outside, b"host").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("planted")).unwrap();

    delete(&root, "old-world").unwrap();
    assert!(root.join("world/level.dat").exists(), "мир на месте");
    assert!(std::fs::symlink_metadata(root.join("old-world")).is_err());

    delete(&root, "planted").unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"host");
    assert!(std::fs::symlink_metadata(root.join("planted")).is_err());

    std::fs::remove_file(outside).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(unix)]
fn renaming_onto_a_link_replaces_the_link() {
    let root = sandbox();
    std::fs::write(root.join("new.txt"), b"new").unwrap();
    std::os::unix::fs::symlink(root.join("world/level.dat"), root.join("target.txt")).unwrap();

    rename(&root, "new.txt", "target.txt").unwrap();
    assert_eq!(std::fs::read(root.join("world/level.dat")).unwrap(), b"lvl");
    assert_eq!(std::fs::read(root.join("target.txt")).unwrap(), b"new");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_long_file_is_read_only_up_to_the_limit() {
    let root = sandbox();
    std::fs::write(root.join("latest.log"), "a".repeat(5000)).unwrap();
    let head = read_text(&root, "latest.log", 1000).unwrap();
    assert!(head.truncated);
    assert_eq!(head.content.len(), 1000);
    assert_eq!(head.size, 5000);

    let whole = read_text(&root, "latest.log", 5000).unwrap();
    assert!(!whole.truncated);
    assert_eq!(whole.content.len(), 5000);
    std::fs::remove_dir_all(root).unwrap();
}
