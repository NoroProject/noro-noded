//! Сон сервера: держатель порта, который будит сервер по первому входу.

use schema::noded::{Listing, OpError};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use super::OpResult;
use crate::server::supervisor;
use crate::Daemon;

pub async fn sleep_placeholder(
    daemon: &Daemon,
    server: Uuid,
    port: u16,
    listing: Box<Listing>,
    enable: bool,
) -> OpResult {
    if !enable {
        // `remove` отдаёт держатель, и его `Drop` отпускает порт:
        // контейнеру он сейчас понадобится.
        daemon.placeholders.remove(&server);
        return Ok(Value::Null);
    }

    // Повторное включение не поднимает второй слушатель на том же
    // порту: мастер может прислать его снова после переподключения.
    if daemon.placeholders.contains_key(&server) {
        return Ok(Value::Null);
    }

    let engine = daemon.engine.clone();
    let registry = daemon.registry.clone();
    let events = daemon.events.clone();
    let placeholders = daemon.placeholders.clone();
    let gate = daemon.wake_gate.clone();

    let held = crate::server::waker::hold(
        server,
        port,
        *listing,
        Arc::new(move |server: Uuid| {
            // Стучать в порт может кто угодно, поэтому здесь два рубежа.
            // Первый — частота, и он же защёлка: проверка памяти ниже
            // асинхронная, плашка на порту всё ещё отвечает, и без
            // отметки времени пачка стуков дала бы пачку запусков.
            if !gate.allow(server) {
                tracing::debug!(%server, "побудка отклонена: слишком часто");
                return;
            }
            let engine = engine.clone();
            let registry = registry.clone();
            let events = events.clone();
            let placeholders = placeholders.clone();
            tokio::spawn(async move {
                // Второй рубеж — память. Переподписка ноды рассчитана на
                // то, что не всё работает разом, а поднять всё разом
                // может любой, кто обойдёт порты спящих серверов.
                if let Some(need) = engine.memory_limit_mb(server).await {
                    let free = crate::metrics::available_memory_mb();
                    if free < need {
                        tracing::warn!(
                            %server, need, free,
                            "побудка отклонена: на ноде нет столько памяти"
                        );
                        return;
                    }
                }
                // Порт отпускается перед запуском, но только когда
                // запуск действительно будет: отпустить и отказать —
                // значит убрать и плашку, и сервер из списка.
                placeholders.remove(&server);
                registry.get_or_create(server).starting_on_purpose();
                if let Err(e) = engine.start(server).await {
                    tracing::warn!(%server, error = %format!("{e:#}"), "разбудить не удалось");
                    return;
                }
                let _ = supervisor::attach(engine, registry, events, server).await;
            });
        }),
    )
    .await
    .map_err(|e| OpError::new("docker_failed", format!("{e:#}")))?;

    daemon.placeholders.insert(server, held);
    Ok(Value::Null)
}
