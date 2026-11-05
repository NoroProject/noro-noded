use super::*;

struct Scratch {
    base: PathBuf,
    root: PathBuf,
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        // Канонический путь: сверенный каталог сравнивает его буквально, а
        // временный каталог на macOS лежит за ссылкой `/var -> /private/var`.
        let base = std::env::temp_dir().join(format!("noded-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let root = base.join("servers").join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(root.join(".noro/backups")).unwrap();
        let dir = base.join("backups").join("server");
        Self { base, root, dir }
    }

    fn legacy(&self, name: &str) -> PathBuf {
        self.root.join(".noro/backups").join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn archives_move_out_of_the_server_directory() {
    let s = Scratch::new();
    let id = Uuid::new_v4();
    std::fs::write(s.legacy(&format!("{id}.tar.gz")), b"tgz").unwrap();

    prepare(&s.dir, &s.root).unwrap();

    assert_eq!(std::fs::read(archive_path(&s.dir, id)).unwrap(), b"tgz");
    assert!(
        !s.root.join(".noro/backups").exists(),
        "пустой прежний каталог убран"
    );
    assert_eq!(usage(&s.dir), 3);
}

/// Прежний каталог принадлежит процессу сервера: ссылка в нём не должна
/// приехать в каталог бэкапов, а чужие файлы — тем более.
#[test]
fn only_our_regular_archives_are_moved() {
    let s = Scratch::new();
    let host = s.base.join("host.txt");
    std::fs::write(&host, "secret").unwrap();
    let linked = Uuid::new_v4();
    std::os::unix::fs::symlink(&host, s.legacy(&format!("{linked}.tar.gz"))).unwrap();
    std::fs::write(s.legacy("notes.txt"), "x").unwrap();

    prepare(&s.dir, &s.root).unwrap();

    assert!(std::fs::symlink_metadata(archive_path(&s.dir, linked)).is_err());
    assert!(s.legacy("notes.txt").exists(), "чужой файл не тронут");
    assert_eq!(usage(&s.dir), 0);
}

#[test]
fn a_linked_legacy_directory_is_left_alone() {
    let s = Scratch::new();
    let outside = s.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let id = Uuid::new_v4();
    std::fs::write(outside.join(format!("{id}.tar.gz")), b"x").unwrap();
    std::fs::remove_dir(s.root.join(".noro/backups")).unwrap();
    std::os::unix::fs::symlink(&outside, s.root.join(".noro/backups")).unwrap();

    assert_eq!(migrate_legacy(&s.root, &s.dir), 0);
    assert!(outside.join(format!("{id}.tar.gz")).exists());
}

#[test]
fn a_linked_backup_directory_is_refused() {
    let s = Scratch::new();
    let outside = s.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(s.dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &s.dir).unwrap();

    assert!(prepare(&s.dir, &s.root).is_err());
}

#[test]
fn an_archive_already_moved_wins_over_the_old_copy() {
    let s = Scratch::new();
    let id = Uuid::new_v4();
    prepare(&s.dir, &s.root).unwrap();
    std::fs::write(archive_path(&s.dir, id), b"new").unwrap();
    std::fs::create_dir_all(s.root.join(".noro/backups")).unwrap();
    std::fs::write(s.legacy(&format!("{id}.tar.gz")), b"old").unwrap();

    migrate_legacy(&s.root, &s.dir);

    assert_eq!(std::fs::read(archive_path(&s.dir, id)).unwrap(), b"new");
    assert!(!s.legacy(&format!("{id}.tar.gz")).exists());
}
