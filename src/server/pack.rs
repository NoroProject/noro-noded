// Файл превышает 150 строк: скачивание пака, правило «что меняет пак, что
// остаётся серверу» и учёт корневых файлов прошлого пака — одно решение,
// разнесённое по файлам, его уже не прочитать целиком.
//! Серверный пак: установка поверх пустого каталога и обновление поверх
//! живого сервера по одному правилу — **пак управляет только своими файлами**.
//!
//! Нода помнит, что положил прошлый пак, и при обновлении удаляет только то из
//! этого, чего нет в новом: мод, убранный из новой версии, иначе остался бы и
//! уронил сервер. Всё, чего пак не приносил, — агент, `noro-chat` и `noro-tab`,
//! моды, поставленные через панель, их конфиги, конфиги, которые моды написали
//! сами, — остаётся, и переставлять его не нужно.
//!
//! Мир, `server.properties`, списки игроков и EULA не трогаются, если уже есть:
//! это нажил сервер, а не привёз пак. Папки вроде `serverutilities/` и
//! `journeymap/`, где настройки пака лежат вперемешку с данными игроков, только
//! дополняются.

use anyhow::{Context, Result};
use schema::noded::PackSource;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::link::master::MasterClient;
use crate::server::layout::Layout;

/// Папки содержимого: в них пак владеет каждым своим файлом.
const OWNED_DIRS: &[&str] = &[
    "mods",
    "coremods",
    "config",
    "defaultconfigs",
    "libraries",
    "scripts",
    "resources",
    "kubejs",
];

/// Нажитое сервером: остаётся как есть, если уже лежит.
const KEPT_FILES: &[&str] = &[
    "server.properties",
    "eula.txt",
    "ops.json",
    "whitelist.json",
    "banned-players.json",
    "banned-ips.json",
    "usercache.json",
    "usernamecache.json",
    "server-icon.png",
];
const KEPT_DIRS: &[&str] = &["logs", "crash-reports", "backups"];

/// Файлы, которые положил прошлый пак. Лежит в корне, а не в `.noro/`: бэкап,
/// перенос и клон служебную папку не берут, а список обязан ехать вместе с
/// файлами, которые описывает, — и откатываться вместе с ними из бэкапа.
const RECORD: &str = ".noro-pack-files.json";
const STAGING: &str = "pack-staging";

/// Что вышло из раскладки.
pub struct Laid {
    pub bytes: u64,
    /// Сервер был, а списка файлов прошлого пака нет — его ставили до того,
    /// как нода начала его вести. Устаревшие файлы пака остались на месте.
    pub untracked: bool,
}

/// Скачать пак с мастера и разложить по серверу.
pub async fn lay_down(master: &MasterClient, layout: &Layout, pack: &PackSource) -> Result<Laid> {
    let staging = layout.service_dir().join(STAGING);
    // Остаток прошлой неудачной попытки — не часть нового пака.
    let _ = tokio::fs::remove_dir_all(&staging).await;
    tokio::fs::create_dir_all(&staging).await?;

    let file = Path::new(&pack.file)
        .file_name()
        .context("у архива пака нет имени")?
        .to_string_lossy()
        .into_owned();
    let bytes = master
        .download(&pack.url, &staging.join(&file), Some(&pack.sha1), None)
        .await
        .context("не скачать серверный пак")?;

    let root = layout.root.clone();
    let untracked = tokio::task::spawn_blocking(move || -> Result<bool> {
        crate::fs::archive::unpack(&staging, &file, "").context("не распаковать пак")?;
        std::fs::remove_file(staging.join(&file))?;
        let untracked = apply(&root, &staging)?;
        std::fs::remove_dir_all(&staging)?;
        Ok(untracked)
    })
    .await??;
    Ok(Laid { bytes, untracked })
}

/// Перенести распакованный пак из `staging` в `root`. Возвращает `true`, если
/// сервер уже был, а списка файлов прошлого пака нет.
pub fn apply(root: &Path, staging: &Path) -> Result<bool> {
    let source = single_top_dir(staging).unwrap_or_else(|| staging.to_path_buf());
    let world = level_name(root);
    let previous = previous_files(root);
    let untracked = previous.is_none() && OWNED_DIRS.iter().any(|d| root.join(d).exists());
    let mut owned = BTreeSet::new();

    for entry in std::fs::read_dir(&source)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let from = entry.path();
        let to = root.join(&name);

        if entry.file_type()?.is_dir() {
            if OWNED_DIRS.contains(&name.as_str()) {
                place(&from, &to, Path::new(&name), &mut owned)?;
            } else if is_kept_dir(&name, &world) && to.exists() {
                continue;
            } else {
                merge_missing(&from, &to)?;
            }
        } else if KEPT_FILES.contains(&name.as_str()) {
            // Свой экземпляр пак кладёт только туда, где пусто, и своим его не
            // считает: следующий пак без ops.json не должен стереть операторов.
            if !to.exists() {
                std::fs::rename(&from, &to)?;
            }
        } else {
            std::fs::rename(&from, &to)?;
            owned.insert(name);
        }
    }

    for stale in previous.unwrap_or_default().difference(&owned) {
        remove_stale(root, stale);
    }
    std::fs::write(root.join(RECORD), serde_json::to_vec(&owned)?)?;
    Ok(untracked)
}

/// Разложить файлы папки содержимого поверх того, что есть, запомнив каждый.
/// Чужие файлы рядом — агент, моды из панели, их конфиги — не трогаются.
fn place(from: &Path, to: &Path, rel: &Path, owned: &mut BTreeSet<String>) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            place(&entry.path(), &target, &rel, owned)?;
        } else {
            std::fs::rename(entry.path(), &target)?;
            owned.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Убрать файл прошлого пака и опустевшие после него папки.
fn remove_stale(root: &Path, rel: &str) {
    let path = root.join(rel);
    if std::fs::remove_file(&path).is_err() {
        return;
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Многие паки упакованы с папкой верхнего уровня; содержимое — в ней.
fn single_top_dir(staging: &Path) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(staging).ok()?.flatten().collect();
    match entries.as_slice() {
        [only] if only.file_type().ok()?.is_dir() => Some(only.path()),
        _ => None,
    }
}

/// Папка мира берётся из `server.properties`: её можно переименовать.
fn level_name(root: &Path) -> String {
    std::fs::read_to_string(root.join("server.properties"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("level-name=").map(str::to_string))
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "world".into())
}

fn is_kept_dir(name: &str, world: &str) -> bool {
    name == world || name.starts_with("world") || KEPT_DIRS.contains(&name)
}

/// Дописать в `to` то, чего там нет; существующее не трогать.
fn merge_missing(from: &Path, to: &Path) -> Result<()> {
    if !to.exists() {
        std::fs::rename(from, to)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            merge_missing(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::rename(entry.path(), target)?;
        }
    }
    Ok(())
}

fn previous_files(root: &Path) -> Option<BTreeSet<String>> {
    std::fs::read(root.join(RECORD))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
