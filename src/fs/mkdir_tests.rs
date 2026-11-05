use super::*;

fn scratch() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("noro-mkdir-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

#[test]
fn missing_levels_are_created_inside_the_root() {
    let root = scratch();
    let target = root.join("world/region/deep");
    let dir = create_dir_all(&root, &target).unwrap();
    assert!(target.is_dir());
    std::fs::write(dir.entry("probe"), b"x").unwrap();
    assert!(target.join("probe").is_file(), "returned dir is the target");
    // Повтор над существующим — не ошибка.
    create_dir_all(&root, &target).unwrap();
}

#[test]
fn a_link_on_the_way_is_refused_and_nothing_lands_outside() {
    let base = scratch();
    let outside = base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let root = base.join("server");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("plugins")).unwrap();

    let err = create_dir_all(&root, &root.join("plugins/evil/deeper"))
        .err()
        .expect("the link is refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        std::fs::read_dir(&outside).unwrap().next().is_none(),
        "a directory was created through the link"
    );
}

#[test]
fn a_path_outside_the_root_is_refused() {
    let base = scratch();
    let root = base.join("server");
    std::fs::create_dir_all(&root).unwrap();
    assert!(create_dir_all(&root, &base.join("elsewhere")).is_err());
    assert!(!base.join("elsewhere").exists());
}
