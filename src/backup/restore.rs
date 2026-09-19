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

    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));

    // Распаковываем поверх: файлы, которых в архиве нет, остаются. Стирать
    // каталог целиком опаснее — в нём могут быть вещи, появившиеся после
    // бэкапа, и владелец ждёт «вернуть мир», а не «снести всё».
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let rel = path.to_string_lossy().replace('\\', "/");

        // Путь из архива — такие же чужие данные, как путь из запроса: архив
        // мог приехать из хранилища, куда писал кто угодно.
        let Ok(target) = crate::fs::resolve(root, &rel) else {
            tracing::warn!(path = %rel, "путь из архива выходит за каталог сервера");
            continue;
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }

    crate::server::layout::chown_recursive(root)?;
    Ok(())
}
