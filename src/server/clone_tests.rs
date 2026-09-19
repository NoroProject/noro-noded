use super::*;
use std::fs;
use std::path::PathBuf;

/// The same throwaway directory the path tests use: pulling `tempfile` in as a
/// dependency for six tests is not worth it, but cleaning up after them is.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "noded-clone-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&base).expect("temp dir");
        Self(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tree(root: &Path, files: &[(&str, &str)]) {
    for (path, body) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, body).unwrap();
    }
}

#[test]
fn mods_and_configs_come_over() {
    let dir = TempDir::new();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();
    tree(
        &from,
        &[
            ("mods/create.jar", "jar"),
            ("config/create/common.toml", "cfg"),
            ("server.properties", "motd=hi"),
        ],
    );

    let report = run(&from, &to, false).unwrap();

    assert!(to.join("mods/create.jar").exists());
    assert!(to.join("config/create/common.toml").exists());
    assert!(to.join("server.properties").exists());
    assert_eq!(report.files, 3);
}

#[test]
fn worlds_stay_behind_unless_asked_for() {
    let dir = TempDir::new();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();
    tree(
        &from,
        &[
            ("world/level.dat", "level"),
            ("world_nether/level.dat", "nether"),
            ("mods/create.jar", "jar"),
        ],
    );

    run(&from, &to, false).unwrap();
    assert!(!to.join("world").exists());
    assert!(!to.join("world_nether").exists());
    assert!(to.join("mods/create.jar").exists());

    run(&from, &to, true).unwrap();
    assert!(to.join("world/level.dat").exists());
    assert!(to.join("world_nether/level.dat").exists());
}

#[test]
fn the_service_directory_is_never_copied() {
    // It holds the agent jar and the token of the source server: carried over,
    // the clone would talk to the master as the original.
    let dir = TempDir::new();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();
    tree(
        &from,
        &[(".noro/agent.jar", "jar"), (".noro/token", "secret")],
    );

    run(&from, &to, true).unwrap();

    assert!(!to.join(".noro").exists());
}

#[test]
fn logs_and_backups_of_the_source_are_left_alone() {
    let dir = TempDir::new();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();
    tree(
        &from,
        &[
            ("logs/latest.log", "log"),
            ("crash-reports/crash.txt", "boom"),
            ("backups/old.tar.gz", "archive"),
            ("eula.txt", "eula=true"),
        ],
    );

    run(&from, &to, true).unwrap();

    assert!(!to.join("logs").exists());
    assert!(!to.join("crash-reports").exists());
    assert!(!to.join("backups").exists());
    assert!(to.join("eula.txt").exists());
}

#[test]
fn a_nested_directory_named_world_is_still_copied() {
    // The skip list applies to the root only: `config/world/settings.toml` is a
    // mod's configuration, not a map.
    let dir = TempDir::new();
    let from = dir.path().join("from");
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();
    tree(&from, &[("config/world/settings.toml", "cfg")]);

    run(&from, &to, false).unwrap();

    assert!(to.join("config/world/settings.toml").exists());
}

#[test]
fn a_missing_source_is_an_error_not_an_empty_copy() {
    let dir = TempDir::new();
    let to = dir.path().join("to");
    fs::create_dir_all(&to).unwrap();

    assert!(run(&dir.path().join("nope"), &to, false).is_err());
}
