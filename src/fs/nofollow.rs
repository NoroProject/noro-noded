// Файл превышает 150 строк: закреплённый каталог и все операции поверх него — одна защита, и врозь её не проверить.
//! Запись и чтение в каталоге сервера, не идя по ссылкам.
//!
//! Каталог сервера примонтирован в контейнер, и всё в нём меняет процесс
//! сервера — то есть код его владельца. Обычная запись или чтение по пути, на
//! месте которого лежит ссылка `-> /etc/passwd`, ушла бы по ней от имени демона,
//! а демон работает с правами root на хосте.
//!
//! Ссылкой может стать не только сам файл, но и любой каталог на пути к нему,
//! и между проверкой пути и операцией его успевают подменить. Поэтому каталог,
//! в котором идёт операция, открывается один раз и сверяется: настоящий путь
//! открытого дескриптора обязан совпасть с тем, что просили. Дальше операция
//! идёт через этот дескриптор (`/proc/self/fd/N/имя`), и подмена пути после
//! сверки её уже не уводит. Вне Linux сверять нечем, и там остаётся проверка,
//! что сам каталог — не ссылка.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Каталог, открытый и сверенный с ожидаемым путём.
pub struct Dir {
    path: PathBuf,
    /// Открытый каталог, если его настоящий путь удалось сверить. `None` —
    /// сверять нечем (нет `/proc`), и операции идут по пути.
    #[cfg(unix)]
    pinned: Option<std::fs::File>,
}

impl Dir {
    /// Открыть каталог, отказав, если он или любой каталог над ним — ссылка.
    ///
    /// Путь сравнивается буквально, поэтому он обязан быть каноническим в
    /// своей неизменной части: каталог данных демон канонизирует при старте,
    /// а пути от разрешателя канонические сами.
    pub fn open(path: &Path) -> std::io::Result<Dir> {
        let path = std::path::absolute(path)?;
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(through_link(&path));
        }
        Self::pin(path)
    }

    #[cfg(unix)]
    fn pin(path: PathBuf) -> std::io::Result<Dir> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(&path)?;
        let real = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()));
        match real {
            Ok(real) if real == path => Ok(Dir {
                path,
                pinned: Some(file),
            }),
            Ok(_) => Err(through_link(&path)),
            // Без /proc (не Linux) сверять нечем. Отказывать во всех записях
            // из-за этого значило бы остановить ноду, поэтому остаётся
            // проверка самого каталога выше.
            Err(_) => Ok(Dir { path, pinned: None }),
        }
    }

    #[cfg(not(unix))]
    fn pin(path: PathBuf) -> std::io::Result<Dir> {
        Ok(Dir { path })
    }

    /// Путь к записи внутри каталога, которым можно пользоваться для любой
    /// операции: он ведёт ровно в сверенный каталог, что бы ни случилось с
    /// путём к нему после.
    pub fn entry(&self, name: impl AsRef<OsStr>) -> PathBuf {
        self.base().join(name.as_ref())
    }

    /// Содержимое каталога — того самого, что сверен при открытии.
    pub fn read_dir(&self) -> std::io::Result<std::fs::ReadDir> {
        std::fs::read_dir(self.base())
    }

    #[cfg(unix)]
    fn base(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        match &self.pinned {
            Some(file) => PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())),
            None => self.path.clone(),
        }
    }

    #[cfg(not(unix))]
    fn base(&self) -> PathBuf {
        self.path.clone()
    }
}

fn through_link(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{} is reached through a link", path.display()),
    )
}

/// Каталог и имя записи в нём.
fn split(path: &Path) -> std::io::Result<(Dir, OsString)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path has no file name",
        )
    })?;
    Ok((Dir::open(parent)?, name.to_owned()))
}

/// Создать каталог, если его нет, и открыть его сверенным.
///
/// Каталог создаётся внутри сверенного родителя, а уже существующий
/// принимается, только если это настоящий каталог, а не ссылка на чужой.
pub fn ensure_dir(path: &Path) -> std::io::Result<Dir> {
    let (parent, name) = split(path)?;
    match std::fs::create_dir(parent.entry(&name)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    Dir::open(path)
}

/// Записать файл целиком.
///
/// Байты ложатся во временный файл, созданный эксклюзивно рядом с целью, и он
/// переименовывается поверх неё. Переименование заменяет саму ссылку, а не то,
/// куда она ведёт, а эксклюзивное создание не идёт и по ссылке, подложенной на
/// место временного файла.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let (dir, name) = split(path).with_context(|| format!("не записать {}", path.display()))?;
    let tmp = {
        let mut tmp = OsString::from(".");
        tmp.push(&name);
        tmp.push(format!(".noro-{}", uuid::Uuid::new_v4().simple()));
        tmp
    };

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.entry(&tmp))
        .with_context(|| format!("не создать временный файл рядом с {}", path.display()))?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|_| std::fs::rename(dir.entry(&tmp), dir.entry(&name))) {
        let _ = std::fs::remove_file(dir.entry(&tmp));
        return Err(e).with_context(|| format!("не записать {}", path.display()));
    }
    Ok(())
}

/// Открыть существующий обычный файл — не ссылку и не то, что успели подложить
/// на её место.
///
/// Проверка ссылки до открытия и открытие — два шага, и между ними файл можно
/// подменить. Поэтому после открытия сверяется, что открылся тот же файл, что
/// был проверен: подменённый путь приводит к другому inode. Обрезать файл, если
/// это нужно, вызывающий должен уже после — через `set_len(0)`.
pub fn open_existing(path: &Path, write: bool) -> std::io::Result<std::fs::File> {
    let (dir, name) = split(path)?;
    let entry = dir.entry(&name);
    let checked = std::fs::symlink_metadata(&entry)?;
    if !checked.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a regular file",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .open(&entry)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if opened.dev() != checked.dev() || opened.ino() != checked.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the file was replaced while being opened",
            ));
        }
    }
    Ok(file)
}

/// Создать новый файл эксклюзивно: на месте имени не должно быть ничего, и
/// ссылки в том числе.
pub fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    let (dir, name) = split(path)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.entry(name))
}

/// Создать файл заново, убрав то, что лежало на его месте, — файл или ссылку.
///
/// Эксклюзивное создание после удаления: подложи процесс сервера ссылку в
/// промежутке, создание откажет, а не пойдёт по ней.
pub fn create_replacing(path: &Path) -> std::io::Result<std::fs::File> {
    let (dir, name) = split(path)?;
    let entry = dir.entry(&name);
    match std::fs::symlink_metadata(&entry) {
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a directory is in the way",
            ))
        }
        Ok(_) => std::fs::remove_file(&entry)?,
        Err(_) => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(entry)
}

/// Удалить запись: файл, ссылку как ссылку или каталог целиком.
pub fn remove(path: &Path) -> std::io::Result<()> {
    let (dir, name) = split(path)?;
    let entry = dir.entry(&name);
    if std::fs::symlink_metadata(&entry)?.is_dir() {
        std::fs::remove_dir_all(entry)
    } else {
        std::fs::remove_file(entry)
    }
}

/// Удалить файл или ссылку, но не каталог.
///
/// Для уборки, которая нашла файл обходом и удаляет его позже: за это время
/// на его месте мог появиться каталог, и сносить его целиком она не просила.
pub fn remove_file(path: &Path) -> std::io::Result<()> {
    let (dir, name) = split(path)?;
    std::fs::remove_file(dir.entry(name))
}

/// Переименовать запись, не идя по ссылкам ни в одном из двух каталогов.
pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from_dir, from_name) = split(from)?;
    let (to_dir, to_name) = split(to)?;
    std::fs::rename(from_dir.entry(from_name), to_dir.entry(to_name))
}

/// Скопировать обычный файл в новый, не идя по ссылкам ни с одной стороны.
pub fn copy_file(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut src = open_existing(from, false)?;
    let mut dst = create_new(to)?;
    std::io::copy(&mut src, &mut dst)
}

/// Прочитать файл целиком, если это обычный файл, а не ссылка.
pub fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    open_existing(path, false)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// То же, текстом.
pub fn read_to_string(path: &Path) -> std::io::Result<String> {
    String::from_utf8(read(path)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(all(test, unix))]
#[path = "nofollow_tests.rs"]
mod tests;
