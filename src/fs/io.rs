//! Чтение, запись и перекладывание файлов сервера.

use anyhow::{bail, Result};
use schema::noded::FileContent;
use std::path::Path;

/// Нулевой байт в начале файла — общепринятый признак, что файл не текстовый:
/// им пользуются `git` и `file`. Точного ответа тут не бывает, а этот
/// ошибается в безопасную сторону — jar, png и zip ловятся все.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Прочитать текстовый файл целиком или его начало.
///
/// Длинный файл обрезается и помечается: панель обязана сказать об этом, иначе
/// сохранение «отредактированного» конфига затрёт рабочий его огрызком.
pub fn read_text(root: &Path, rel: &str, max_bytes: u64) -> Result<FileContent> {
    guard_hidden(rel)?;
    let path = super::resolve_existing(root, rel)?;
    let meta = std::fs::metadata(&path)?;
    let limit = max_bytes.min(super::MAX_TEXT_BYTES);

    let bytes = std::fs::read(&path)?;

    // Содержимое двоичного файла не отдаём вовсе: показать jar «текстом» мало
    // того что бесполезно — сохранение такого «текста» уничтожит файл.
    if looks_binary(&bytes) {
        return Ok(FileContent {
            path: rel.to_string(),
            content: String::new(),
            size: meta.len(),
            truncated: false,
            binary: true,
        });
    }

    let truncated = meta.len() > limit;
    let slice = if truncated {
        &bytes[..limit as usize]
    } else {
        &bytes[..]
    };

    Ok(FileContent {
        path: rel.to_string(),
        // from_utf8_lossy, а не отказ: конфиг с одним битым байтом всё равно
        // надо показать и починить, а не прятать за ошибкой.
        content: String::from_utf8_lossy(slice).into_owned(),
        size: meta.len(),
        truncated,
        binary: false,
    })
}

/// Записать файл, создав недостающие каталоги.
///
/// Пишем во временный файл рядом и переименовываем: прерванная запись иначе
/// оставляет обрезанный `server.properties`, с которым сервер не стартует.
pub fn write_text(root: &Path, rel: &str, content: &str) -> Result<()> {
    guard_hidden(rel)?;
    let path = super::resolve(root, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        // Существующий путь проверяем строже: за это время там мог появиться
        // симлинк наружу.
        super::resolve_existing(root, rel)?;
    }

    let tmp = path.with_extension(format!(
        "{}.noro-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn mkdir(root: &Path, rel: &str) -> Result<()> {
    guard_hidden(rel)?;
    let path = super::resolve(root, rel)?;
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Удалить файл или каталог целиком.
pub fn delete(root: &Path, rel: &str) -> Result<()> {
    guard_hidden(rel)?;
    if rel.trim_matches('/').is_empty() {
        bail!("корень сервера не удаляется");
    }
    let path = super::resolve(root, rel)?;
    let meta = std::fs::symlink_metadata(&path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        // Симлинк удаляем как есть: снять подложенную ссылку должно быть можно,
        // даже если ходить по ней нельзя.
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn rename(root: &Path, from: &str, to: &str) -> Result<()> {
    guard_hidden(from)?;
    guard_hidden(to)?;
    let src = super::resolve_existing(root, from)?;
    let dst = super::resolve(root, to)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(src, dst)?;
    Ok(())
}

fn guard_hidden(rel: &str) -> Result<()> {
    if super::is_hidden(rel) {
        bail!("служебный каталог недоступен");
    }
    Ok(())
}
