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
