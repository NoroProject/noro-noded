//! Подключение к докеру и общие для всех операций мелочи.

use anyhow::{Context, Result};
use bollard::Docker;

/// Префикс имени контейнера. Демон трогает только свои: всё, что назвали иначе,
/// на этой машине не его дело, даже если выглядит похоже.
pub const NAME_PREFIX: &str = "noro-";

/// Метка с идентификатором сервера. По ней контейнер узнаётся после
/// перезапуска демона, даже если имя кто-то поменял руками.
pub const LABEL_SERVER: &str = "dev.noro.server";
pub const LABEL_MANAGED: &str = "dev.noro.managed";
pub const LABEL_PRIMARY_PORT: &str = "dev.noro.primary_port";

/// Куда примонтирован каталог сервера внутри контейнера.
pub const CONTAINER_ROOT: &str = "/home/container";

/// Кем контейнер бегает.
///
/// `eclipse-temurin` по умолчанию рутовый, и тогда сервер пишет файлы от root,
/// а демон потом не может их ни удалить, ни отдать по SFTP. Чинится это уже
/// рекурсивным chown всей ноды, поэтому uid задаётся сразу и совпадает с тем,
/// под которым работает сам демон.
pub const CONTAINER_UID: u32 = 1000;
pub const CONTAINER_GID: u32 = 1000;

#[derive(Clone)]
pub struct Engine {
    pub docker: Docker,
}

impl Engine {
    pub fn connect(socket: &str) -> Result<Self> {
        let docker = Docker::connect_with_unix(socket, 120, bollard::API_DEFAULT_VERSION)
            .with_context(|| format!("не подключиться к докеру через {socket}"))?;
        Ok(Self { docker })
    }

    pub async fn version(&self) -> Result<String> {
        let v = self.docker.version().await.context("докер не отвечает")?;
        Ok(v.version.unwrap_or_default())
    }

    pub fn container_name(server: uuid::Uuid) -> String {
        format!("{NAME_PREFIX}{server}")
    }

    /// Идентификатор сервера из метки контейнера.
    pub fn server_of(
        labels: Option<&std::collections::HashMap<String, String>>,
    ) -> Option<uuid::Uuid> {
        labels?
            .get(LABEL_SERVER)
            .and_then(|v| uuid::Uuid::parse_str(v).ok())
    }
}

/// Образы, которые демон соглашается запускать.
///
/// Строка из API до докера доходить не должна: произвольный образ — это чужой
/// код с нашим сокетом под боком. Список намеренно узкий; расширять его —
/// осознанное решение, а не побочный эффект поля в форме.
pub fn image_allowed(image: &str) -> bool {
    const ALLOWED_PREFIXES: &[&str] = &["eclipse-temurin:", "ghcr.io/noroproject/"];
    ALLOWED_PREFIXES.iter().any(|p| image.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_images_are_allowed() {
        assert!(image_allowed("eclipse-temurin:21-jre"));
        assert!(image_allowed("ghcr.io/noroproject/java:21"));
        assert!(!image_allowed("alpine"));
        assert!(!image_allowed("evil/miner:latest"));
        // Похожее начало не считается: имя реестра целиком или ничего.
        assert!(!image_allowed("notghcr.io/noroproject/java:21"));
    }

    #[test]
    fn container_name_is_derived_from_the_id() {
        let id = uuid::Uuid::nil();
        assert_eq!(
            Engine::container_name(id),
            "noro-00000000-0000-0000-0000-000000000000"
        );
    }
}
