//! Запись и чтение в каталоге сервера, не идя по ссылкам.
//!
//! Каталог сервера примонтирован в контейнер, и всё в нём меняет процесс
//! сервера — то есть код его владельца. Обычная запись или чтение по пути, на
//! месте которого лежит ссылка `-> /etc/passwd`, ушла бы по ней от имени демона,
//! а демон работает с правами root на хосте.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;

/// Записать файл целиком.
///
/// Байты ложатся во временный файл, созданный эксклюзивно рядом с целью, и он
/// переименовывается поверх неё. Переименование заменяет саму ссылку, а не то,
/// куда она ведёт, а эксклюзивное создание не идёт и по ссылке, подложенной на
/// место временного файла. Каталог, в котором лежит цель, ссылкой быть не может.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("у пути нет имени файла")?;
    if std::fs::symlink_metadata(parent)?.file_type().is_symlink() {
        bail!(
            "каталог {} — ссылка, писать в него нельзя",
            parent.display()
        );
    }

    let tmp = parent.join(format!(".{name}.noro-{}", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("не создать {}", tmp.display()))?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
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
    let checked = std::fs::symlink_metadata(path)?;
    if !checked.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not a regular file",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .open(path)?;
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

/// Создать файл заново, убрав то, что лежало на его месте, — файл или ссылку.
///
/// Эксклюзивное создание после удаления: подложи процесс сервера ссылку в
/// промежутке, создание откажет, а не пойдёт по ней.
pub fn create_replacing(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a directory is in the way",
            ))
        }
        Ok(_) => std::fs::remove_file(path)?,
        Err(_) => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Скопировать обычный файл в новый, не идя по ссылкам ни с одной стороны.
pub fn copy_file(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut src = open_existing(from, false)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
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
