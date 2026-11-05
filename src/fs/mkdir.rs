//! Создание каталогов внутри каталога сервера — по дескрипторам, а не по пути.
//!
//! `create_dir_all(путь)` проходит путь заново на каждом уровне. Процесс
//! сервера, подменивший каталог посередине ссылкой `-> /etc`, получал каталоги,
//! созданные демоном с правами root там, куда ссылка вела, — а следом туда же
//! ложился распакованный архив или залитый файл.
//!
//! Здесь спуск идёт от проверенного корня: каждый уровень создаётся `mkdirat`
//! в уже открытом родителе и открывается `openat` с `O_NOFOLLOW`. Ссылка на
//! любом уровне — отказ, а подменить уже открытого родителя нельзя: дальше
//! операции идут через его дескриптор, не через путь.

use std::ffi::OsStr;
use std::path::{Component, Path};

use super::nofollow::Dir;

/// Создать `path` со всеми недостающими каталогами, спускаясь от `root`.
///
/// `path` обязан лежать внутри `root` — так его возвращает разрешатель путей.
/// Возвращает сверенный каталог `path`: в нём и продолжать работу, а не
/// открывать путь заново.
pub fn create_dir_all(root: &Path, path: &Path) -> std::io::Result<Dir> {
    let root = root.canonicalize()?;
    let rel = path.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is outside {}", path.display(), root.display()),
        )
    })?;
    let mut parts = Vec::new();
    for part in rel.components() {
        match part {
            Component::Normal(name) => parts.push(name),
            Component::CurDir => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the path leaves its directory",
                ))
            }
        }
    }
    walk(&root, &parts)?;
    Dir::open(path)
}

#[cfg(unix)]
fn walk(root: &Path, parts: &[&OsStr]) -> std::io::Result<()> {
    let mut dir = Dir::open(root)?.fd()?;
    for name in parts {
        make_at(&dir, name)?;
        dir = open_dir_at(Some(&dir), name)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn walk(root: &Path, parts: &[&OsStr]) -> std::io::Result<()> {
    std::fs::create_dir_all(parts.iter().fold(root.to_path_buf(), |p, n| p.join(n)))
}

/// `mkdirat`: уже существующий каталог — не ошибка, решит следующий `openat`.
#[cfg(unix)]
fn make_at(parent: &std::os::fd::OwnedFd, name: &OsStr) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = c_name(name)?;
    // SAFETY: дескриптор открыт и жив до конца вызова, имя — строка с нулём.
    let done = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
    if done == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::AlreadyExists {
        Ok(())
    } else {
        Err(err)
    }
}

/// Открыть каталог `name` в `parent` (или по пути, если родителя нет), не идя
/// по ссылке. Ссылка или файл на этом месте — отказ.
#[cfg(unix)]
pub(super) fn open_dir_at(
    parent: Option<&std::os::fd::OwnedFd>,
    name: &OsStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let c = c_name(name)?;
    let at = parent.map_or(libc::AT_FDCWD, |p| p.as_raw_fd());
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: дескриптор родителя жив до конца вызова, имя — строка с нулём.
    let fd = unsafe { libc::openat(at, c.as_ptr(), flags) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        // ELOOP — ссылка на последнем шаге, ENOTDIR — файл или ссылка не на
        // каталог: и то и другое значит, что путь ведёт не туда.
        return Err(match err.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is reached through a link", Path::new(name).display()),
            ),
            _ => err,
        });
    }
    // SAFETY: `openat` только что вернул этот дескриптор, и владелец у него один.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(unix)]
fn c_name(name: &OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a NUL in the name"))
}

#[cfg(all(test, unix))]
#[path = "mkdir_tests.rs"]
mod tests;
