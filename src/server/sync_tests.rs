//! Проверки уборки после синка.
//!
//! Скачивание здесь не проверяется — для него нужен мастер. Проверяется то,
//! что дороже: какие файлы синк трогает, а какие нет.

use super::*;
use std::fs;
use std::path::PathBuf;

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("noded-sync-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::create_dir_all(root.join("world/region")).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::create_dir_all(root.join(".noro")).unwrap();

        fs::write(root.join("mods/jei.jar"), "a").unwrap();
        fs::write(root.join("mods/old-mod.jar"), "b").unwrap();
        fs::write(root.join("world/region/r.0.0.mca"), "world").unwrap();
        fs::write(root.join("world/level.dat"), "level").unwrap();
        fs::write(root.join("logs/latest.log"), "log").unwrap();
        fs::write(root.join(".noro/agent-secret"), "secret").unwrap();
        fs::write(root.join("server.properties"), "port").unwrap();

        Self { root }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn policy(prune: bool) -> SyncPolicy {
    SyncPolicy {
        managed_roots: vec!["mods".into(), "config".into()],
        keep: vec![".noro".into(), "logs".into(), "server.properties".into()],
        prune,
    }
}

fn wanted(paths: &[&str]) -> HashSet<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

/// Главная проверка всего файла: мир переживает синк при любых настройках.
#[test]
fn a_world_is_never_touched() {
    let s = Sandbox::new();
    let mut report = SyncReport::default();

    prune(
        &s.root,
        &policy(true),
        &wanted(&["mods/jei.jar"]),
        &mut report,
    );

    assert!(
        s.root.join("world/region/r.0.0.mca").exists(),
        "регион мира"
    );
    assert!(s.root.join("world/level.dat").exists(), "level.dat");
    assert!(
        !report.extra.iter().any(|p| p.starts_with("world/")),
        "мир даже не попадает в «лишнее»: {:?}",
        report.extra
    );
}

#[test]
fn stale_mods_go_away_but_only_inside_managed_roots() {
    let s = Sandbox::new();
    let mut report = SyncReport::default();

    prune(
        &s.root,
        &policy(true),
        &wanted(&["mods/jei.jar"]),
        &mut report,
    );

    assert!(
        !s.root.join("mods/old-mod.jar").exists(),
        "мод не из сборки"
    );
    assert!(s.root.join("mods/jei.jar").exists(), "мод из сборки");
    assert_eq!(report.deleted, 1);
}

/// В сборке агента нет и быть не должно, а лежит он ровно там, где убирают
/// лишнее. Без этой защиты каждая раскатка молча отключала бы сервер от мастера.
#[test]
fn the_agent_is_not_swept_away_by_a_build_rollout() {
    let s = Sandbox::new();
    fs::write(s.root.join("mods/noro-agent.jar"), "jar").unwrap();
    let mut report = SyncReport::default();

    prune(&s.root, &policy(true), &wanted(&[]), &mut report);

    assert!(
        s.root.join("mods/noro-agent.jar").exists(),
        "агент на месте"
    );
}

#[test]
fn kept_paths_survive_even_inside_a_managed_root() {
    let s = Sandbox::new();
    fs::write(s.root.join("mods/.noro-marker"), "x").unwrap();
    let mut report = SyncReport::default();

    let mut p = policy(true);
    p.keep.push("mods/.noro-marker".into());
    prune(&s.root, &p, &wanted(&[]), &mut report);

    assert!(s.root.join("mods/.noro-marker").exists());
}

/// Служебный каталог и логи не трогаются: в первом лежит секрет агента, второй
/// — единственное, по чему разбирают падение.
#[test]
fn service_files_and_logs_stay() {
    let s = Sandbox::new();
    let mut report = SyncReport::default();

    prune(&s.root, &policy(true), &wanted(&[]), &mut report);

    assert!(s.root.join(".noro/agent-secret").exists());
    assert!(s.root.join("logs/latest.log").exists());
    assert!(s.root.join("server.properties").exists());
}

/// Без уборки ничего не удаляется, но лишнее видно: панель показывает список,
/// а решение остаётся за человеком.
#[test]
fn without_prune_nothing_is_deleted_and_extras_are_reported() {
    let s = Sandbox::new();
    let mut report = SyncReport::default();

    prune(
        &s.root,
        &policy(false),
        &wanted(&["mods/jei.jar"]),
        &mut report,
    );

    assert!(s.root.join("mods/old-mod.jar").exists());
    assert_eq!(report.deleted, 0);
    assert_eq!(report.extra, vec!["mods/old-mod.jar".to_string()]);
}

#[test]
fn an_unchanged_file_is_recognised_by_size_and_hash() {
    let s = Sandbox::new();
    let path = s.root.join("mods/jei.jar");
    let entry = FileEntry {
        path: "mods/jei.jar".into(),
        sha1: file_sha1(&path).unwrap(),
        size: 1,
        url: "https://example.invalid/a".into(),
        side: schema::build::FileSide::Server,
        executable: false,
        platform: None,
    };

    assert!(up_to_date(&path, &entry));

    let changed = FileEntry {
        sha1: "0".repeat(40),
        ..entry
    };
    assert!(
        !up_to_date(&path, &changed),
        "хеш разошёлся — файл качается"
    );
}
