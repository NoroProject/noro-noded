//! Правка `server.properties` до первого старта.
//!
//! Формат построчный и с комментариями, поэтому файл не пересобирается: чужие
//! ключи и порядок сохраняются, меняются только названные. Иначе первая же
//! установка стирала бы настройки, которые владелец правил руками.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub fn apply(path: &Path, changes: &BTreeMap<String, String>) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut remaining = changes.clone();
    let mut out = String::with_capacity(existing.len() + 128);

    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || !trimmed.contains('=') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        match remaining.remove(key) {
            Some(value) => {
                out.push_str(&format!("{key}={value}\n"));
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    for (key, value) in remaining {
        out.push_str(&format!("{key}={value}\n"));
    }

    std::fs::write(path, out)?;
    Ok(())
}

/// Согласиться с EULA. Сервер без этого файла пишет строку в лог и выходит, а
/// выглядит это как «не запускается».
pub fn accept_eula(path: &Path) -> Result<()> {
    std::fs::write(path, "# accepted by the Noro panel\neula=true\n")?;
    Ok(())
}

/// Гарантировать, что в `server.properties` выставлен правильный выделенный порт.
/// Без этого распакованная сборка (например, с `server-port=25565`) или сервер,
/// создавший файл при первом запуске, слушает не тот порт, на который докер
/// пробрасывает трафик, и снаружи выглядит как `Connection refused`.
pub fn ensure_port(path: &Path, port: u16) -> Result<()> {
    let mut changes = BTreeMap::new();
    changes.insert("server-port".into(), port.to_string());
    changes.insert("query.port".into(), port.to_string());
    changes.insert("server-ip".into(), String::new());
    apply(path, &changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn existing_keys_are_replaced_in_place() {
        let dir = std::env::temp_dir().join(format!("noded-props-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.properties");
        std::fs::write(
            &path,
            "#Minecraft server properties\nserver-port=25565\nmotd=Hello\nview-distance=10\n",
        )
        .unwrap();

        apply(
            &path,
            &changes(&[("server-port", "25600"), ("online-mode", "true")]),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert!(text.contains("server-port=25600"));
        assert!(text.contains("motd=Hello"), "чужие ключи остаются");
        assert!(text.contains("view-distance=10"));
        assert!(text.starts_with("#Minecraft"), "комментарий на месте");
        assert!(text.contains("online-mode=true"), "новый ключ дописан");
        assert_eq!(text.matches("server-port=").count(), 1, "без дублей");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_created_from_the_changes() {
        let dir = std::env::temp_dir().join(format!("noded-props-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.properties");

        apply(&path, &changes(&[("server-port", "25565")])).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "server-port=25565\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_port_rewrites_server_port_and_clears_server_ip() {
        let dir = std::env::temp_dir().join(format!("noded-props-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.properties");
        std::fs::write(
            &path,
            "server-ip=127.0.0.1\nserver-port=25565\nquery.port=25565\nmotd=Custom\n",
        )
        .unwrap();

        ensure_port(&path, 25500).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert!(text.contains("server-port=25500"));
        assert!(text.contains("query.port=25500"));
        assert!(text.contains("server-ip=\n"));
        assert!(text.contains("motd=Custom"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
