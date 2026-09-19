// Файл превышает 150 строк: одна функция разрешения пути и разбор всех форм побега рядом с ней.
//! Единственное место, где относительный путь превращается в путь на диске.
//!
//! Точек входа две — HTTP-операции от мастера и SFTP-сессия, — а реализация
//! обязана быть одна. Две расходятся на первом же нетривиальном случае, и
//! расходятся молча: проверка, которая пропускает лишнее, ничем себя не выдаёт
//! до того дня, когда ей воспользуются.
//!
//! Проверяется не строка, а результат. Строку можно отфильтровать от `..` и всё
//! равно выйти наружу через симлинк, подложенный файловым менеджером часом
//! раньше, поэтому существующая часть пути канонизируется и сверяется с корнем.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("путь выходит за каталог сервера")]
    Escapes,
    #[error("путь недопустим")]
    Invalid,
}

pub type Result<T> = std::result::Result<T, PathError>;

/// Каталоги, которые сервер не отдаёт наружу ни при каких правах.
///
/// `.noro` держит секрет агента, authlib-injector и служебные отметки: и
/// читать, и перезаписывать его нельзя, иначе доступ к файлам превращается в
/// доступ к секрету игрового сервера.
const HIDDEN: &[&str] = &[".noro"];

pub fn is_hidden(rel: &str) -> bool {
    let head = rel.trim_start_matches('/').split('/').next().unwrap_or("");
    HIDDEN.contains(&head)
}

/// Разрешить относительный путь внутри корня.
///
/// Возвращает путь, которым можно пользоваться: существующая его часть заведомо
/// лежит внутри корня даже после разворачивания симлинков. Несуществующий хвост
/// сохраняется — иначе нельзя было бы создать файл.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let cleaned = clean(rel)?;
    let canonical_root = root.canonicalize().map_err(|_| PathError::Invalid)?;
    let candidate = canonical_root.join(&cleaned);

    // Самый глубокий существующий предок — то, что вообще можно канонизировать.
    let mut existing = candidate.as_path();
    loop {
        if existing.exists() {
            break;
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return Err(PathError::Invalid),
        }
    }

    let real = existing.canonicalize().map_err(|_| PathError::Invalid)?;
    if !real.starts_with(&canonical_root) {
        return Err(PathError::Escapes);
    }

    // Хвост после существующей части не может содержать симлинков — их ещё нет.
    let tail = candidate
        .strip_prefix(existing)
        .map_err(|_| PathError::Invalid)?;
    // Пустой хвост не приклеивается: `join("")` дописывает разделитель, и
    // существующий файл превращается в `file.jar/`, который система открывать
    // отказывается — ENOTDIR вместо содержимого.
    let resolved = if tail.as_os_str().is_empty() {
        real
    } else {
        real.join(tail)
    };

    // Куда путь привёл **на самом деле**. Проверять по запрошенной строке
    // недостаточно: ссылка `mods/x.jar -> .noro/secret` формально просится в
    // `mods/`, а открывает служебный каталог с секретом игрового сервера.
    if is_hidden(&relative_to(&canonical_root, &resolved)) {
        return Err(PathError::Escapes);
    }

    Ok(resolved)
}

/// То же, но путь обязан существовать и сам не быть симлинком.
///
/// Проверяется **запрошенный** путь до канонизации: после неё ссылка уже
/// развёрнута и неотличима от обычного файла. Ссылка внутрь корня наружу не
/// выводит, но открывать надо файл, а не её.
pub fn resolve_existing(root: &Path, rel: &str) -> Result<PathBuf> {
    let cleaned = clean(rel)?;
    let canonical_root = root.canonicalize().map_err(|_| PathError::Invalid)?;
    let raw = canonical_root.join(&cleaned);

    let meta = std::fs::symlink_metadata(&raw).map_err(|_| PathError::Invalid)?;
    if meta.file_type().is_symlink() {
        return Err(PathError::Escapes);
    }

    resolve(root, rel)
}

/// Разобрать относительный путь, отвергнув всё, чем из корня выходят.
fn clean(rel: &str) -> Result<PathBuf> {
    if rel.contains('\0') {
        return Err(PathError::Invalid);
    }

    let mut cleaned = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            // Отвергаем, а не отбрасываем: `..` в запросе — это либо ошибка
            // клиента, либо попытка, и молча превращать её в другой путь
            // значит скрыть и то, и другое.
            Component::ParentDir => return Err(PathError::Escapes),
            Component::Prefix(_) | Component::RootDir => return Err(PathError::Invalid),
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
        }
    }
    Ok(cleaned)
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
#[path = "safe_tests.rs"]
mod tests;
