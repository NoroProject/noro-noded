//! Что уже крутится на машине.
//!
//! Контейнеры переживают перезапуск демона — и это правильно: падение
//! управляющего процесса не повод ронять чужую игру. Поэтому при старте демон
//! пересчитывает свои контейнеры заново и рассказывает мастеру, что нашёл.

use anyhow::Result;
use bollard::query_parameters::ListContainersOptionsBuilder;
use schema::noded::{PowerState, ServerSnapshot};
use std::collections::HashMap;

use super::engine::{Engine, LABEL_MANAGED};

/// Найденный контейнер: либо наш сервер, либо сирота.
pub struct Found {
    pub server: Option<uuid::Uuid>,
    pub name: String,
    pub id: String,
    pub state: PowerState,
}

impl Engine {
    /// Все контейнеры с нашей меткой, запущенные и нет.
    pub async fn list_managed(&self) -> Result<Vec<Found>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_MANAGED}=true")]);

        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();

        let list = self.docker.list_containers(Some(options)).await?;
        Ok(list
            .into_iter()
            .map(|c| Found {
                server: Engine::server_of(c.labels.as_ref()),
                name: c
                    .names
                    .and_then(|n| n.first().cloned())
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string(),
                id: c.id.unwrap_or_default(),
                state: state_of(c.state.as_ref()),
            })
            .collect())
    }

    /// Снимок для кадра `hello`.
    pub async fn snapshot(&self) -> Result<Vec<ServerSnapshot>> {
        let mut out = Vec::new();
        for found in self.list_managed().await? {
            let Some(server) = found.server else { continue };
            let (state, _exit, uptime) = self.state(server).await.unwrap_or((found.state, None, 0));
            out.push(ServerSnapshot {
                server,
                state,
                // Готовность известна только тому, кто читал консоль от старта.
                // После перезапуска демона честнее сказать «не знаю» и дать
                // серверу подтвердить это следующей строкой лога.
                ready: false,
                uptime_secs: uptime,
                container: true,
            });
        }
        Ok(out)
    }
}

fn state_of(state: Option<&bollard::models::ContainerSummaryStateEnum>) -> PowerState {
    use bollard::models::ContainerSummaryStateEnum as S;
    match state {
        Some(S::RUNNING) => PowerState::Running,
        Some(S::RESTARTING) => PowerState::Starting,
        Some(S::DEAD) => PowerState::Crashed,
        _ => PowerState::Offline,
    }
}
