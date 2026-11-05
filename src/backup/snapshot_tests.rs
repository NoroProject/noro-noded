// Файл превышает 150 строк: свойства хранилища кусков проверяются вместе — нарезка, дедупликация, сборка мусора, восстановление и обрывы.
use super::*;
use crate::backup::gc;
use crate::backup::restore::{unpack_snapshot, Filter};
use crate::backup::store;
use rand::{Rng, SeedableRng};
use std::path::PathBuf;

pub(super) struct Scratch {
    pub base: PathBuf,
    pub root: PathBuf,
    pub dir: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        // Канонический путь: сверенный каталог сравнивает его буквально, а
        // временный каталог на macOS лежит за ссылкой `/var -> /private/var`.
        let base = std::env::temp_dir().join(format!("noded-snap-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let root = base.join("server");
        std::fs::create_dir_all(root.join("world/region")).unwrap();
        let dir = base.join("backups");
        Self { base, root, dir }
    }

    pub fn repo(&self) -> Repo {
        Repo::open(&self.dir).unwrap()
    }

    pub fn take(&self, limit: Option<u64>) -> Result<(Uuid, BackupResult)> {
        let id = Uuid::new_v4();
        take(&self.root, &self.repo(), id, &[], limit).map(|r| (id, r))
    }

    pub fn restored(&self, backup: Uuid, paths: &[String]) -> PathBuf {
        let out = self.base.join(format!("restored-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&out).unwrap();
        unpack_snapshot(&out, &self.repo(), backup, &Filter::new(paths)).unwrap();
        out
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Несжимаемые байты, как у регионов: zstd не должен скрывать дедупликацию.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rand::rngs::StdRng::seed_from_u64(seed).fill(&mut out[..]);
    out
}

fn chunks_of(data: &[u8]) -> Vec<ChunkId> {
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| ChunkId::of(&data[c.offset..c.offset + c.length]))
        .collect()
}

pub(super) fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (rel, std::fs::read(e.path()).unwrap())
        })
        .collect();
    out.sort();
    out
}

/// Вставка в середину файла меняет куски только вокруг себя: границы по
/// содержимому догоняют сдвиг, а нарезка поровну сдвинула бы все следующие.
#[test]
fn an_insert_in_the_middle_changes_few_chunks() {
    let before = noise(1, 16 * 1024 * 1024);
    let mut after = before.clone();
    after.splice(8_000_000..8_000_000, noise(2, 3000));

    let old: std::collections::HashSet<_> = chunks_of(&before).into_iter().collect();
    let new = chunks_of(&after);
    let changed = new.iter().filter(|c| !old.contains(c)).count();
    assert!(
        new.len() >= 8,
        "кусков слишком мало для проверки: {}",
        new.len()
    );
    assert!(changed <= 2, "поменялось {changed} кусков из {}", new.len());
}

#[test]
fn chunk_sizes_stay_within_bounds() {
    let data = noise(3, 24 * 1024 * 1024);
    let cuts: Vec<_> =
        fastcdc::v2020::FastCDC::new(&data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).collect();
    for c in &cuts[..cuts.len() - 1] {
        assert!(c.length >= MIN_CHUNK as usize && c.length <= MAX_CHUNK as usize);
    }
    let avg = data.len() / cuts.len();
    assert!(
        (512 * 1024..=2 * 1024 * 1024).contains(&avg),
        "средний кусок {avg}"
    );
}

/// Второй снимок мира, где поменялся один регион из четырёх, добавляет на
/// диск примерно этот регион, а не ещё одну копию мира.
#[test]
fn a_second_snapshot_stores_only_what_changed() {
    let s = Scratch::new();
    for i in 0..4 {
        std::fs::write(
            s.root.join(format!("world/region/r.{i}.0.mca")),
            noise(i, 4 << 20),
        )
        .unwrap();
    }
    std::fs::write(s.root.join("server.properties"), b"motd=hi").unwrap();

    let (_, first) = s.take(None).unwrap();
    assert!(
        first.bytes >= 16 << 20,
        "первый снимок хранит мир: {}",
        first.bytes
    );
    assert_eq!(first.source_bytes, Some((16 << 20) + 7));

    let mut region = std::fs::read(s.root.join("world/region/r.2.0.mca")).unwrap();
    region[2_000_000..2_004_096].copy_from_slice(&noise(9, 4096));
    std::fs::write(s.root.join("world/region/r.2.0.mca"), region).unwrap();

    let (_, second) = s.take(None).unwrap();
    assert!(
        second.bytes < 6 << 20,
        "второй снимок хранит {} байт вместо одного-двух кусков",
        second.bytes
    );
    // Квота считает место хранилища, а не сумму снимков.
    let used = store::usage(&s.dir);
    assert!(used < first.bytes + second.bytes + 4096, "{used}");
    assert!(used >= first.bytes, "{used}");
}

#[test]
fn an_unchanged_tree_adds_only_the_manifest() {
    let s = Scratch::new();
    std::fs::write(s.root.join("world/region/r.0.0.mca"), noise(4, 3 << 20)).unwrap();
    s.take(None).unwrap();
    let (_, again) = s.take(None).unwrap();
    assert!(again.bytes < 4096, "{}", again.bytes);
}

/// Удалённый снимок уносит только свои куски: соседний восстанавливается
/// побайтно, а место под удалённым освобождается.
#[test]
fn removing_a_snapshot_collects_only_its_chunks() {
    let s = Scratch::new();
    std::fs::write(s.root.join("world/region/r.0.0.mca"), noise(5, 3 << 20)).unwrap();
    std::fs::write(s.root.join("world/old.dat"), noise(6, 2 << 20)).unwrap();
    let (first, _) = s.take(None).unwrap();

    std::fs::remove_file(s.root.join("world/old.dat")).unwrap();
    std::fs::write(s.root.join("world/new.dat"), noise(7, 1 << 20)).unwrap();
    let (second, _) = s.take(None).unwrap();
    let expected = tree(&s.root);

    let repo = s.repo();
    assert!(repo.remove_snapshot(first).unwrap());
    let before = store::usage(&s.dir);
    let done = gc::collect(&repo).unwrap();
    assert!(done.freed >= 2 << 20, "{done:?}");
    assert_eq!(store::usage(&s.dir), before - done.freed);

    let out = s.restored(second, &[]);
    assert_eq!(tree(&out), expected);
}

#[test]
fn a_restore_is_byte_for_byte_and_keeps_modes() {
    let s = Scratch::new();
    std::fs::write(s.root.join("world/region/r.0.0.mca"), noise(8, 5 << 20)).unwrap();
    std::fs::write(s.root.join("start.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::create_dir_all(s.root.join("plugins/empty")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        s.root.join("start.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (id, _) = s.take(None).unwrap();

    let out = s.restored(id, &[]);
    assert_eq!(tree(&out), tree(&s.root));
    assert!(
        out.join("plugins/empty").is_dir(),
        "пустой каталог вернулся"
    );
    let mode = std::fs::metadata(out.join("start.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn a_single_path_restores_alone() {
    let s = Scratch::new();
    std::fs::write(s.root.join("world/region/r.0.0.mca"), noise(10, 1 << 20)).unwrap();
    std::fs::create_dir_all(s.root.join("plugins/x")).unwrap();
    std::fs::write(s.root.join("plugins/x/config.yml"), b"a: 1").unwrap();
    std::fs::write(s.root.join("plugins/xy.jar"), b"jar").unwrap();
    let (id, _) = s.take(None).unwrap();

    let out = s.restored(id, &["/plugins/x/".into()]);
    let names: Vec<_> = tree(&out).into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        names,
        vec!["plugins/x/config.yml"],
        "соседний plugins/xy.jar не задет"
    );

    let empty = s.base.join("nothing");
    std::fs::create_dir_all(&empty).unwrap();
    let err = unpack_snapshot(&empty, &s.repo(), id, &Filter::new(&["nope".into()]));
    assert!(
        err.is_err(),
        "несуществующий путь — отказ, а не тихий успех"
    );
}

/// Ссылка в папке сервера не уносит в снимок файл хоста, а жёсткую ссылку
/// формат не умеет выразить вовсе — она снимается как обычный файл.
#[test]
fn links_in_the_server_directory_stay_out() {
    let s = Scratch::new();
    std::fs::write(s.base.join("noded.toml"), b"token=secret").unwrap();
    std::os::unix::fs::symlink(s.base.join("noded.toml"), s.root.join("cfg.toml")).unwrap();
    std::os::unix::fs::symlink(&s.base, s.root.join("host")).unwrap();
    let (id, _) = s.take(None).unwrap();

    let manifest = s.repo().manifest(id).unwrap();
    let paths: Vec<_> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with("cfg") || p.starts_with("host")),
        "{paths:?}"
    );
}

/// Манифест на диске — чужие данные: путь наружу и путь через подложенную в
/// папку сервера ссылку не выводят восстановление за её пределы.
#[test]
fn a_tampered_manifest_cannot_leave_the_server_directory() {
    let s = Scratch::new();
    std::fs::write(s.root.join("level.dat"), b"lvl").unwrap();
    let (id, _) = s.take(None).unwrap();

    let repo = s.repo();
    let mut manifest = repo.manifest(id).unwrap();
    let body = manifest
        .entries
        .iter()
        .find(|e| e.path == "level.dat")
        .unwrap()
        .clone();
    for path in ["../escaped.txt", "/etc/escaped.txt", "out/escaped.txt"] {
        let mut evil = body.clone();
        evil.path = path.into();
        manifest.entries.push(evil);
    }
    repo.commit(&manifest).unwrap();

    let out = s.base.join("target");
    std::fs::create_dir_all(&out).unwrap();
    let outside = s.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, out.join("out")).unwrap();

    unpack_snapshot(&out, &repo, id, &Filter::new(&[])).unwrap();
    assert_eq!(std::fs::read(out.join("level.dat")).unwrap(), b"lvl");
    assert!(!s.base.join("escaped.txt").exists());
    assert!(!outside.join("escaped.txt").exists());
}

/// Снимок, переросший отведённое место, обрывается, и после него в
/// хранилище не остаётся ни манифеста, ни его кусков.
#[test]
fn a_snapshot_over_its_budget_leaves_nothing() {
    let s = Scratch::new();
    std::fs::write(s.root.join("world/region/r.0.0.mca"), noise(11, 2 << 20)).unwrap();

    let err = s.take(Some(512 * 1024)).unwrap_err();
    assert!(err.downcast_ref::<OverBudget>().is_some(), "{err:#}");
    let repo = s.repo();
    assert!(repo.snapshots().unwrap().is_empty());
    assert!(!repo.interrupted());
    assert_eq!(store::usage(&s.dir), 0);
}

/// Демон умер посреди упаковки: куски легли, манифеста нет, рядом брошен
/// временный файл. Следующая упаковка убирает всё это до того, как считать
/// место, а прежние снимки не трогает.
#[test]
fn an_interrupted_snapshot_is_cleaned_by_the_next() {
    let s = Scratch::new();
    std::fs::write(s.root.join("level.dat"), noise(12, 300 * 1024)).unwrap();
    let (kept, _) = s.take(None).unwrap();

    let repo = s.repo();
    repo.begin(Uuid::new_v4()).unwrap();
    let orphan = noise(13, 400 * 1024);
    let id = ChunkId::of(&orphan);
    repo.put_chunk(&id, &orphan, None).unwrap();
    let leftover = repo.chunk_path(&id).with_file_name(format!(
        ".{}.noro-{}",
        id.hex(),
        Uuid::new_v4().simple()
    ));
    std::fs::write(&leftover, b"half").unwrap();

    s.take(None).unwrap();
    assert!(!repo.has_chunk(&id), "ничей кусок убран");
    assert!(!leftover.exists(), "брошенный временный файл убран");
    assert!(!repo.interrupted());
    assert_eq!(tree(&s.restored(kept, &[])), tree(&s.root));
}

/// Пропавший кусок обнаруживается до первой записи в папку сервера.
#[test]
fn a_missing_chunk_stops_the_restore_before_it_starts() {
    let s = Scratch::new();
    std::fs::write(s.root.join("a.dat"), noise(14, 300 * 1024)).unwrap();
    std::fs::write(s.root.join("b.dat"), noise(15, 300 * 1024)).unwrap();
    let (id, _) = s.take(None).unwrap();
    let repo = s.repo();
    let manifest = repo.manifest(id).unwrap();
    let b = manifest.entries.iter().find(|e| e.path == "b.dat").unwrap();
    std::fs::remove_file(repo.chunk_path(&b.chunks[0].id)).unwrap();

    let out = s.base.join("target");
    std::fs::create_dir_all(&out).unwrap();
    assert!(unpack_snapshot(&out, &repo, id, &Filter::new(&[])).is_err());
    assert!(!out.join("a.dat").exists(), "ни одного файла не записано");
}

/// Испорченный на диске кусок не ложится в мир молча.
#[test]
fn a_corrupted_chunk_is_refused() {
    let s = Scratch::new();
    std::fs::write(s.root.join("a.dat"), noise(16, 300 * 1024)).unwrap();
    let (id, _) = s.take(None).unwrap();
    let repo = s.repo();
    let chunk = repo.manifest(id).unwrap().entries[0].chunks[0];
    let other = zstd::bulk::compress(&noise(17, chunk.len as usize), 3).unwrap();
    std::fs::write(repo.chunk_path(&chunk.id), other).unwrap();
    assert!(repo.read_chunk(&chunk).is_err());
}

/// Нечитаемый манифест останавливает сборку мусора: иначе куски снимка,
/// который просто не разобрался, ушли бы как ничьи.
#[test]
fn an_unreadable_manifest_stops_the_collector() {
    let s = Scratch::new();
    std::fs::write(s.root.join("a.dat"), noise(18, 300 * 1024)).unwrap();
    let (id, _) = s.take(None).unwrap();
    let repo = s.repo();
    let chunk = repo.manifest(id).unwrap().entries[0].chunks[0].id;
    std::fs::write(repo.snapshot_path(id), b"garbage").unwrap();
    assert!(gc::collect(&repo).is_err());
    assert!(repo.has_chunk(&chunk));
}
