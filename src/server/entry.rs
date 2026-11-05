//! Чем запускать сервер: jar в корне или файл аргументов установщика.

use std::path::Path;

/// Чем сервер запускается на самом деле.
///
/// Мастер присылает то, что записано в карточке, а правда лежит на диске: имя
/// мог сменить установщик, а карточка — остаться с `server.jar`. Пересобрать
/// контейнер под несуществующий файл значит сломать работающий сервер правкой
/// лимита памяти.
///
/// Если на диске не нашлось ничего похожего, оставляем присланное: отказ при
/// старте с внятной строкой в логе честнее, чем подставленный наугад файл.
pub fn resolve_entry_point(root: &Path, wanted: &str) -> String {
    // Патч-рантайм новой Java для 1.7.10 (lwjgl3ify / GTNH) безусловно заменяет
    // Forge-jar: он мог распаковаться из сервер-пака уже после установщика Forge.
    if root.join("lwjgl3ify-forgePatches.jar").is_file() {
        return "lwjgl3ify-forgePatches.jar".to_string();
    }
    // Если на диске есть unix_args.txt (Forge/NeoForge 1.17+), он всегда выигрывает
    // у обёрток вроде `minecraft_server.jar` или `server.jar`.
    if wanted.is_empty() || wanted == "minecraft_server.jar" || wanted == "server.jar" {
        if let Some(installed) = find_installed_jar(root) {
            if installed.starts_with('@') {
                return installed;
            }
        }
    }
    let exists = match wanted.strip_prefix('@') {
        Some(rel) => root.join(rel).is_file(),
        None => !wanted.is_empty() && root.join(wanted).is_file(),
    };
    if exists {
        return wanted.to_string();
    }
    match find_installed_jar(root) {
        Some(found) => {
            tracing::info!(wanted, found, "точка входа найдена на диске");
            found
        }
        None => wanted.to_string(),
    }
}

/// Что запускать после установщика.
///
/// NeoForge кладёт файл аргументов, Forge — свой jar, и угадывать имя по версии
/// нельзя: оно менялось между поколениями загрузчика не раз.
pub fn find_installed_jar(root: &Path) -> Option<String> {
    // Файл аргументов ищем по имени, а не по пути: он лежит под версией
    // загрузчика, а его номер знает установщик, не мы.
    let args = walkdir::WalkDir::new(root.join("libraries"))
        .max_depth(6)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name() == "unix_args.txt")
        .and_then(|e| e.path().strip_prefix(root).ok().map(|p| p.to_path_buf()));
    if let Some(rel) = args {
        return Some(format!("@{}", rel.display()));
    }

    // В корне ищем то, чем сервер вообще может быть: имена у ядер разные —
    // `paper-1.21.1-133.jar`, `fabric-server-launch.jar`, `forge-1.16.5-….jar`,
    // `server.jar`. Установщик и клиентский jar исключаются явно.
    let mut candidates: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            let lower = name.to_lowercase();
            lower.ends_with(".jar") && !lower.contains("installer") && !lower.contains("client")
        })
        .collect();

    // Порядок разбора важен: загрузчики выигрывают у ванильного minecraft_server.jar.
    candidates.sort();
    let rank = |name: &str| {
        let lower = name.to_lowercase();
        match () {
            _ if lower.starts_with("lwjgl3ify") || lower.contains("retrofutura") => 0,
            _ if lower == "server.jar" => 1,
            _ if lower.contains("forge") && lower.contains("universal") => 2,
            _ if lower.contains("forge") => 3,
            _ if lower.contains("fabric") => 4,
            _ if lower.contains("paper") || lower.contains("purpur") => 5,
            _ if lower.starts_with("minecraft_server") => 10,
            _ if lower.contains("server") => 6,
            _ => 7,
        }
    };
    candidates.into_iter().min_by_key(|name| rank(name))
}
