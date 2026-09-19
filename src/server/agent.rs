//! Игровой агент рядом с сервером.
//!
//! Тот же jar, что ставит Java-враппер, только кладёт его демон, а настройки
//! уходят переменными окружения контейнера: `AgentConfig` читает
//! `NORO_MASTER_URL` и `NORO_AGENT_SECRET` раньше файла, поэтому конфиг писать
//! не нужно и секрет не оседает на диске сервера.

use anyhow::{bail, Context, Result};
use schema::noded::AgentSpec;
use std::path::{Path, PathBuf};

use crate::link::master::MasterClient;

/// Имя одно и то же во всех установках: переустановка обязана заменить старый
/// jar, а не положить второй рядом — два агента на одном сервере передерутся
/// за один и тот же игровой сервер на мастере.
pub const JAR_NAME: &str = "noro-agent.jar";

/// Куда агент ложится относительно корня сервера.
pub fn jar_path(root: &Path, dir: &str) -> PathBuf {
    root.join(dir).join(JAR_NAME)
}

/// Относительный путь — им же синк сборки защищает агент от уборки.
pub fn rel_path(dir: &str) -> String {
    format!("{dir}/{JAR_NAME}")
}

pub async fn install(master: &MasterClient, root: &Path, spec: &AgentSpec) -> Result<u64> {
    // Каталог выбирает мастер по платформе, но приходит он строкой: пускать её
    // в путь без проверки — это запись куда угодно по дереву сервера.
    if spec.dir != "mods" && spec.dir != "plugins" {
        bail!("агент просится в каталог {}", spec.dir);
    }

    let path = jar_path(root, &spec.dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    master
        .download(&spec.url, &path, Some(&spec.sha1), None)
        .await
        .context("не скачать агент")
}
