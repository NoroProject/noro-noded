// Файл превышает 150 строк: каталог сервера и смена его владельца — одно правило о том, что принадлежит контейнеру.
//! Каталог сервера на диске.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Служебный подкаталог внутри сервера.
///
/// Лежит **внутри** маунта намеренно: authlib-injector и jar агента должны
/// быть видны контейнеру, а значит обязаны попасть в тот единственный каталог,
/// который туда примонтирован. Наружу он при этом закрыт — ни файловый
/// менеджер, ни SFTP, ни бэкап, ни синк сборки его не трогают.
pub const SERVICE_DIR: &str = ".noro";

pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn service_dir(&self) -> PathBuf {
        self.root.join(SERVICE_DIR)
    }

    pub fn authlib_jar(&self) -> PathBuf {
        self.service_dir().join("authlib-injector.jar")
    }

    /// Путь к тому же файлу внутри контейнера.
    pub fn container_authlib_jar() -> String {
        format!(
            "{}/{SERVICE_DIR}/authlib-injector.jar",
            crate::docker::CONTAINER_ROOT
        )
    }

    /// Создать каталог и отдать его тому uid, под которым бегает контейнер.
    ///
    /// Без chown сервер пишет мир от своего пользователя, а демон потом не
    /// может ни удалить файл, ни отдать его по SFTP. Чинится это уже
    /// рекурсивным chown всей ноды — дешевле сделать сразу.
    pub fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(self.service_dir())?;
        chown_recursive(&self.root)?;
        Ok(())
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join(SERVICE_DIR).join("backups")
    }

    pub fn primary_port_file(&self) -> PathBuf {
        self.service_dir().join("primary_port")
    }

    pub fn read_primary_port(&self) -> Option<u16> {
        crate::fs::nofollow::read_to_string(&self.primary_port_file())
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
    }

    pub fn write_primary_port(&self, port: u16) -> Result<()> {
        std::fs::create_dir_all(self.service_dir())?;
        crate::fs::nofollow::write(&self.primary_port_file(), port.to_string().as_bytes())?;
        Ok(())
    }
}

/// Отдать каталог сервера uid контейнера.
///
/// Только `lchown`: каталог сервера меняет процесс в контейнере, и ссылка
/// `x -> /var/run/docker.sock` в нём — обычное дело для злоумышленника. `chown`
/// пошёл бы по ней и отдал файл хоста uid контейнера, а на многих машинах этот
/// uid — первый пользователь хоста, то есть сокет докера и с ним root.
///
/// Одного `lchown` мало: обход идёт секундами, часто по живому серверу, и
/// каталог посреди пути успевали подменить ссылкой — `lchown` по пути
/// `plugins/x/passwd` уходил в `/etc/passwd`. Поэтому каждый каталог
/// открывается и сверяется (`nofollow::Dir`), а владелец меняется у записей
/// внутри него, а не по пути от корня.
#[cfg(unix)]
pub fn chown_recursive(path: &Path) -> Result<()> {
    use crate::docker::engine::{CONTAINER_GID, CONTAINER_UID};
    use std::os::unix::fs::lchown;

    lchown(path, Some(CONTAINER_UID), Some(CONTAINER_GID))?;
    let dir = crate::fs::nofollow::Dir::open(path)?;
    chown_tree(&dir, path, 0);
    Ok(())
}

/// Глубже этого каталоги серверу не нужны, а без потолка рекурсия по нарочно
/// вложенным папкам роняла бы демон переполнением стека.
#[cfg(unix)]
const MAX_DEPTH: usize = 128;

#[cfg(unix)]
fn chown_tree(dir: &crate::fs::nofollow::Dir, path: &Path, depth: usize) {
    use crate::docker::engine::{CONTAINER_GID, CONTAINER_UID};
    use std::os::unix::fs::{lchown, MetadataExt};

    // Ошибку на одной записи не превращаем в отказ целиком: каталог мог
    // измениться прямо во время обхода, и это нормально.
    let Ok(entries) = dir.read_dir() else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(meta) = entry.metadata() else { continue };
        // Файл с несколькими именами мог получить второе имя вне сервера —
        // так выглядит жёсткая ссылка на файл хоста. Своему файлу сервера
        // смена владельца не нужна: его и создал процесс сервера.
        if !meta.is_dir() && meta.nlink() > 1 {
            continue;
        }
        let _ = lchown(dir.entry(&name), Some(CONTAINER_UID), Some(CONTAINER_GID));
        if meta.is_dir() && depth < MAX_DEPTH {
            let child = path.join(&name);
            if let Ok(sub) = crate::fs::nofollow::Dir::open(&child) {
                chown_tree(&sub, &child, depth + 1);
            }
        }
    }
}

#[cfg(not(unix))]
pub fn chown_recursive(_path: &Path) -> Result<()> {
    Ok(())
}

/// Один файл: залитый через прямую передачу ложится от имени демона, а читает
/// его сервер внутри контейнера — под другим uid.
#[cfg(unix)]
pub fn chown_path(path: &Path) -> Result<()> {
    use crate::docker::engine::{CONTAINER_GID, CONTAINER_UID};
    use std::os::unix::fs::lchown;

    // `lchown` в сверенном каталоге по той же причине, что и в
    // `chown_recursive`: между записью файла и сменой владельца на его место
    // или на место каталога над ним могли подложить ссылку.
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        anyhow::bail!("у пути нет имени файла");
    };
    let dir = crate::fs::nofollow::Dir::open(parent)?;
    lchown(dir.entry(name), Some(CONTAINER_UID), Some(CONTAINER_GID))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn chown_path(_path: &Path) -> Result<()> {
    Ok(())
}
