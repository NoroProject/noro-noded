// Файл превышает 150 строк: распаковка и её проверка на подложенных ссылках рядом — без теста правило не удержать.
//! Восстановление из архива.
//!
//! Сервер перед этим обязан быть остановлен: распаковывать мир под работающим
//! сервером значит получить смесь старых и новых регионов — хуже, чем не
//! восстанавливать вовсе. Решение принимает мастер, здесь — только отказ, если
//! контейнер всё ещё жив.

use anyhow::{bail, Result};
use flate2::read::GzDecoder;
use std::path::Path;

use crate::docker::Engine;
use crate::server::registry::Registry;

pub async fn restore(
    engine: &Engine,
    registry: &Registry,
    server: uuid::Uuid,
    root: &Path,
    archive: &Path,
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

    if !archive.exists() {
        bail!("архива нет на этой ноде");
    }

    // Распаковка тоже блокирующая и тоже на гигабайты — на рабочем потоке
    // рантайма она заморозила бы всю ноду, а не только этот сервер.
    let (root, archive) = (root.to_path_buf(), archive.to_path_buf());
    tokio::task::spawn_blocking(move || unpack_into(&root, &archive))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("распаковка не выполнена: {e}")))
}

fn unpack_into(root: &Path, archive: &Path) -> Result<()> {
    unpack_files(root, archive)?;
    crate::server::layout::chown_recursive(root)?;
    Ok(())
}

fn unpack_files(root: &Path, archive: &Path) -> Result<()> {
    // Архив лежит в служебном каталоге, а его процесс сервера тоже может
    // переписать: открывается он так же, как любой файл сервера.
    let file = crate::fs::nofollow::open_existing(archive, false)?;
    let mut tar = tar::Archive::new(GzDecoder::new(std::io::BufReader::new(file)));

    // Распаковываем поверх: файлы, которых в архиве нет, остаются. Стирать
    // каталог целиком опаснее — в нём могут быть вещи, появившиеся после
    // бэкапа, и владелец ждёт «вернуть мир», а не «снести всё».
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

        // Путь из архива — такие же чужие данные, как путь из запроса.
        let Ok(target) = crate::fs::resolve(root, &rel) else {
            tracing::warn!(path = %rel, "путь из архива выходит за каталог сервера");
            continue;
        };
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = crate::fs::nofollow::create_replacing(&target)?;
        std::io::copy(&mut entry, &mut out)?;
        keep_metadata(&out, entry.header());
    }
    Ok(())
}

/// Права и время изменения из архива. Без прав скрипт запуска из бэкапа
/// перестаёт быть исполняемым; setuid и прочие старшие биты не переносятся.
fn keep_metadata(file: &std::fs::File, header: &tar::Header) {
    #[cfg(unix)]
    if let Ok(mode) = header.mode() {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777));
    }
    if let Ok(mtime) = header.mtime() {
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

        unpack_files(&root, &tgz).unwrap();

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
