//! Питание контейнера: старт, мягкая остановка, снятие, удаление.

use anyhow::Result;
use bollard::query_parameters::{
    KillContainerOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
};
use schema::noded::PowerState;

use super::engine::Engine;

/// Сколько ждать, пока сервер сохранится и выйдет сам.
///
/// Столько же, сколько у Java-враппера: большой мир с сотней игроков пишется
/// десятки секунд, и убить его на половине сохранения — это повреждённые
/// регионы вместо корректного выключения.
const STOP_TIMEOUT_SECS: i32 = 60;

impl Engine {
    pub async fn start(&self, server: uuid::Uuid) -> Result<()> {
        self.docker
            .start_container(&Engine::container_name(server), None)
            .await?;
        Ok(())
    }

    /// Мягкая остановка: докер шлёт SIGTERM и ждёт. Сама команда `stop` в
    /// консоль отправляется выше — там, где известно, живой ли сервер.
    pub async fn stop(&self, server: uuid::Uuid) -> Result<()> {
        let options = StopContainerOptionsBuilder::default()
            .t(STOP_TIMEOUT_SECS)
            .build();
        if let Err(e) = self
            .docker
            .stop_container(&Engine::container_name(server), Some(options))
            .await
        {
            if !self.already_down(server).await {
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub async fn kill(&self, server: uuid::Uuid) -> Result<()> {
        let options = KillContainerOptionsBuilder::default()
            .signal("SIGKILL")
            .build();
        if let Err(e) = self
            .docker
            .kill_container(&Engine::container_name(server), Some(options))
            .await
        {
            if !self.already_down(server).await {
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Контейнер существует, но не работает.
    ///
    /// Упавший сервер остаётся контейнером в состоянии `exited`, и докер на
    /// «останови» отвечает отказом: `kill` — «container is not running», `stop`
    /// — «not modified». Показывать это человеку незачем: он просил, чтобы
    /// сервер не работал, и сервер не работает. Несуществующий контейнер под
    /// это не подходит — там отказ по делу, и его мы отдаём как есть.
    async fn already_down(&self, server: uuid::Uuid) -> bool {
        matches!(self.state(server).await, Ok((power, ..)) if !power.is_up())
    }

    pub async fn remove(&self, server: uuid::Uuid) -> Result<()> {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            // Тома не трогаем: каталог сервера — bind, и снести его удалением
            // контейнера было бы худшим видом «уборки».
            .v(false)
            .build();
        self.docker
            .remove_container(&Engine::container_name(server), Some(options))
            .await?;
        Ok(())
    }

    /// Текущее состояние по данным докера.
    ///
    /// `OOMKilled` разбирается отдельно: «убит лимитом памяти» и «упал» —
    /// разные поломки, и на неправильной из них уходит день.
    pub async fn state(&self, server: uuid::Uuid) -> Result<(PowerState, Option<i32>, u64)> {
        let info = self
            .docker
            .inspect_container(&Engine::container_name(server), None)
            .await?;

        let Some(state) = info.state else {
            return Ok((PowerState::Offline, None, 0));
        };

        let exit_code = state.exit_code.map(|c| c as i32);
        let uptime = state
            .started_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| {
                (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                    .num_seconds()
                    .max(0) as u64
            })
            .unwrap_or(0);

        let running = state.running.unwrap_or(false);
        let power = if running {
            PowerState::Running
        } else if state.oom_killed.unwrap_or(false) {
            PowerState::OomKilled
        } else if exit_code.unwrap_or(0) != 0 {
            PowerState::Crashed
        } else {
            PowerState::Offline
        };

        Ok((power, exit_code, uptime))
    }

    /// Сколько памяти контейнеру разрешено занять.
    ///
    /// `None` — контейнера ещё нет либо лимит не задан, и тогда о допуске судить
    /// не по чему: отказывать на этом основании было бы отказом наугад.
    pub async fn memory_limit_mb(&self, server: uuid::Uuid) -> Option<i64> {
        let info = self
            .docker
            .inspect_container(&Engine::container_name(server), None)
            .await
            .ok()?;
        let bytes = info.host_config?.memory?;
        (bytes > 0).then_some(bytes / 1024 / 1024)
    }

    /// Работает ли контейнер прямо сейчас.
    ///
    /// Отдельно от `state`: пересборке нужен один бит, а не разбор причины
    /// падения, и контейнера может не быть вовсе.
    pub async fn was_running(&self, server: uuid::Uuid) -> bool {
        matches!(self.state(server).await, Ok((power, ..)) if power.is_up())
    }

    pub async fn exists(&self, server: uuid::Uuid) -> bool {
        self.docker
            .inspect_container(&Engine::container_name(server), None)
            .await
            .is_ok()
    }
}
