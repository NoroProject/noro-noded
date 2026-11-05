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
#[cfg(unix)]
pub fn chown_recursive(path: &Path) -> Result<()> {
    use crate::docker::engine::{CONTAINER_GID, CONTAINER_UID};
    use std::os::unix::fs::lchown;

    lchown(path, Some(CONTAINER_UID), Some(CONTAINER_GID))?;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        // Ошибку на одном файле не превращаем в отказ целиком: каталог мог
        // измениться прямо во время обхода, и это нормально.
        let _ = lchown(entry.path(), Some(CONTAINER_UID), Some(CONTAINER_GID));
    }
    Ok(())
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

    // `lchown` по той же причине, что и в `chown_recursive`: между записью
    // файла и сменой владельца на его место могли подложить ссылку.
    lchown(path, Some(CONTAINER_UID), Some(CONTAINER_GID))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn chown_path(_path: &Path) -> Result<()> {
    Ok(())
}
