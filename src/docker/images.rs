//! Скачивание образов.

use anyhow::{bail, Result};
use bollard::query_parameters::CreateImageOptionsBuilder;
use futures_util::StreamExt;

use super::engine::{image_allowed, Engine};

impl Engine {
    /// Забрать образ, если его ещё нет.
    ///
    /// Проверка списка стоит здесь, а не только при создании контейнера:
    /// «просто скачать» чужой образ — это уже произвольный слой на диске ноды.
    pub async fn pull_image(&self, image: &str) -> Result<()> {
        if !image_allowed(image) {
            bail!("образ {image} не из разрешённого списка");
        }
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }

        let options = CreateImageOptionsBuilder::default()
            .from_image(image)
            .build();
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(step) = stream.next().await {
            // Прогресс не пересылаем: он приходит десятками кадров в секунду и
            // панели ничего не добавляет — важен только итог.
            step?;
        }
        Ok(())
    }
}
