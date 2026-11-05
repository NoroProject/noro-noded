// Файл превышает 150 строк: распаковка и её проверка на подложенных ссылках рядом — без теста правило не удержать.
//! Восстановление из архива.
//!
//! Сервер перед этим обязан быть остановлен: распаковывать мир под работающим
//! сервером значит получить смесь старых и новых регионов — хуже, чем не
//! восстанавливать вовсе. Решение принимает мастер, здесь — только отказ, если
//! контейнер всё ещё жив.

use anyhow::{bail, Result};
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::repo::Repo;
use super::stream::ChunkReader;
use crate::docker::Engine;
use crate::server::registry::Registry;

/// Из чего восстанавливать.
pub enum Source {
    /// Полный `.tar.gz`: прежние бэкапы и скачанные из хранилища.
    Archive(PathBuf),
    /// Снимок в хранилище кусков.
    Snapshot { repo: PathBuf, backup: uuid::Uuid },
}

/// Восстановить сервер. `paths` — только эти файлы и каталоги; пусто — всё.
pub async fn restore(
    engine: &Engine,
    registry: &Registry,
    server: uuid::Uuid,
    root: &Path,
    source: Source,
    paths: Vec<String>,
) -> Result<()> {
    if let Some(handle) = registry.get(server) {
        if handle.state().power.is_up() {
            bail!("сервер должен быть остановлен до восстановления");
        }
    }
    // Спрашиваем ещё и докер: реестр в памяти мог отстать — например, демон
    // только что перезапустился и ещё не переподключился к контейнеру.
    if let Ok((power, _, _)) = engine.state(server).await {
        if power.is_up() {
            bail!("контейнер сервера всё ещё работает");
        }
    }

    // Распаковка тоже блокирующая и тоже на гигабайты — на рабочем потоке
    // рантайма она заморозила бы всю ноду, а не только этот сервер.
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || unpack_into(&root, &source, &Filter::new(&paths)))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("распаковка не выполнена: {e}")))
}

fn unpack_into(root: &Path, source: &Source, filter: &Filter) -> Result<()> {
    match source {
        Source::Archive(archive) => {
            if std::fs::symlink_metadata(archive).is_err() {
                bail!("архива нет на этой ноде");
            }
            unpack_files(root, archive, filter)?
        }
        Source::Snapshot { repo, backup } => {
            unpack_snapshot(root, &Repo::at(repo), *backup, filter)?
        }
    }
    crate::server::layout::chown_recursive(root)?;
    Ok(())
}

/// Какие пути восстанавливать: сам путь и всё под ним.
pub(super) struct Filter(Vec<String>);

impl Filter {
    pub fn new(paths: &[String]) -> Self {
        Self(
            paths
                .iter()
                .map(|p| p.trim_matches('/').trim_start_matches("./").to_string())
                .filter(|p| !p.is_empty() && p != ".")
                .collect(),
        )
    }

    /// Пусто — восстанавливается весь бэкап.
    pub fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    pub fn wants(&self, rel: &str) -> bool {
        let rel = rel.trim_end_matches('/');
        self.0.is_empty()
            || self.0.iter().any(|p| {
                rel == p
                    || (rel.starts_with(p.as_str()) && rel.as_bytes().get(p.len()) == Some(&b'/'))
            })
    }
}

fn unpack_files(root: &Path, archive: &Path, filter: &Filter) -> Result<()> {
    // Архив мог приехать из хранилища или от прежней раскладки, когда он
    // лежал в папке сервера: открывается он так же, как любой файл сервера.
    let file = crate::fs::nofollow::open_existing(archive, false)?;
    let mut tar = tar::Archive::new(GzDecoder::new(std::io::BufReader::new(file)));

    // Распаковываем поверх: файлы, которых в архиве нет, остаются. Стирать
    // каталог целиком опаснее — в нём могут быть вещи, появившиеся после
    // бэкапа, и владелец ждёт «вернуть мир», а не «снести всё».
    let mut placed = 0usize;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        // Только файлы и каталоги: наш бэкап ничего другого не содержит. Архив
        // же мог подменить процесс сервера или приехать из хранилища, и тогда
        // жёсткая ссылка в нём становилась ссылкой на файл хоста, а chown всего
        // каталога после распаковки отдавал этот файл uid контейнера.
        if !kind.is_file() && !kind.is_dir() {
            continue;
        }
        let rel = entry.path()?.to_string_lossy().replace('\\', "/");
        if !filter.wants(&rel) {
            continue;
        }
        let mode = entry.header().mode().ok();
        let mtime = entry.header().mtime().ok();
        if kind.is_dir() {
            place_dir(root, &rel)?;
        } else {
            place_file(root, &rel, &mut entry, mode, mtime)?;
        }
        placed += 1;
    }
    nothing_found(filter, placed)
}

/// Выборочное восстановление, не нашедшее ни одного пути, — это опечатка в
/// пути, а не успех: владелец иначе решит, что файл вернулся.
fn nothing_found(filter: &Filter, placed: usize) -> Result<()> {
    if placed == 0 && !filter.is_all() {
        bail!("в бэкапе нет такого пути");
    }
    Ok(())
}

pub(super) fn unpack_snapshot(
    root: &Path,
    repo: &Repo,
    backup: uuid::Uuid,
    filter: &Filter,
) -> Result<()> {
    let manifest = repo.manifest(backup)?;
    let wanted: Vec<_> = manifest
        .entries
        .iter()
        .filter(|e| filter.wants(&e.path))
        .collect();
    nothing_found(filter, wanted.len())?;
    // Все куски на месте — до первой записи в папку сервера: оборваться на
    // середине из-за пропавшего куска значит оставить мир наполовину старым.
    if let Some(lost) = wanted
        .iter()
        .flat_map(|e| e.chunks.iter())
        .find(|c| !repo.has_chunk(&c.id))
    {
        bail!("в хранилище нет куска {:?} — снимок неполон", lost.id);
    }
    for entry in wanted {
        if entry.dir {
            place_dir(root, &entry.path)?;
            continue;
        }
        let mut body = ChunkReader::new(repo, entry);
        place_file(
            root,
            &entry.path,
            &mut body,
            Some(entry.mode),
            Some(entry.mtime.max(0) as u64),
        )?;
    }
    Ok(())
}

/// Путь из бэкапа — такие же чужие данные, как путь из запроса: архив мог
/// приехать из хранилища, а снимок — лечь на диск чужими руками.
fn place_dir(root: &Path, rel: &str) -> Result<()> {
    let Ok(target) = crate::fs::resolve(root, rel) else {
        tracing::warn!(path = %rel, "путь из бэкапа выходит за каталог сервера");
        return Ok(());
    };
    crate::fs::mkdir::create_dir_all(root, &target)?;
    Ok(())
}

fn place_file(
    root: &Path,
    rel: &str,
    body: &mut impl Read,
    mode: Option<u32>,
    mtime: Option<u64>,
) -> Result<()> {
    let Ok(target) = crate::fs::resolve(root, rel) else {
        tracing::warn!(path = %rel, "путь из бэкапа выходит за каталог сервера");
        return Ok(());
    };
    if let Some(parent) = target.parent() {
        crate::fs::mkdir::create_dir_all(root, parent)?;
    }
    let mut out = crate::fs::nofollow::create_replacing(&target)?;
    std::io::copy(body, &mut out)?;
    keep_metadata(&out, mode, mtime);
    Ok(())
}

/// Права и время изменения из бэкапа. Без прав скрипт запуска из бэкапа
/// перестаёт быть исполняемым; setuid и прочие старшие биты не переносятся.
fn keep_metadata(file: &std::fs::File, mode: Option<u32>, mtime: Option<u64>) {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Some(mtime) = mtime {
        let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn archive(
        path: &Path,
        build: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<std::fs::File>>),
    ) {
        let file = std::fs::File::create(path).unwrap();
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::fast(),
        ));
        build(&mut tar);
        tar.into_inner().unwrap().finish().unwrap();
    }

    /// Жёсткая и мягкая ссылки из архива не создаются: обе ведут куда угодно.
    #[test]
    fn links_in_an_archive_are_not_restored() {
        let base = std::env::temp_dir().join(format!("noded-restore-{}", uuid::Uuid::new_v4()));
        let root = base.join("server");
        std::fs::create_dir_all(&root).unwrap();
        let host = base.join("host.txt");
        std::fs::write(&host, "host").unwrap();
        let tgz = base.join("b.tar.gz");

        archive(&tgz, |tar| {
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "world/level.dat", &b"lvl"[..])
                .unwrap();

            for kind in [tar::EntryType::Link, tar::EntryType::Symlink] {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(kind);
                header.set_size(0);
                let name = format!("{kind:?}.txt");
                tar.append_link(&mut header, name, &host).unwrap();
            }
        });

        unpack_files(&root, &tgz, &Filter::new(&[])).unwrap();

        assert_eq!(std::fs::read(root.join("world/level.dat")).unwrap(), b"lvl");
        assert!(std::fs::symlink_metadata(root.join("Link.txt")).is_err());
        assert!(std::fs::symlink_metadata(root.join("Symlink.txt")).is_err());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("world/level.dat"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "права из архива сохранились");
        std::fs::remove_dir_all(base).unwrap();
    }
}
