// Файл превышает 150 строк: присмотр за запуском и автоподъём после падения — один цикл жизни сервера.
//! Присмотр за запущенным сервером: консоль наружу и смена состояния.

use anyhow::Result;
use schema::noded::{NodeEvent, PowerState};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::docker::console::{self, ConsoleBatch};
use crate::docker::Engine;
use crate::server::registry::{Registry, ServerHandle};

/// Сколько раз подряд поднимать упавший сервер.
///
/// Потолок обязателен: сервер, который падает на старте из-за битого мода,
/// иначе перезапускается вечно и заваливает лог и мастера событиями.
const MAX_RESTARTS: u32 = 3;

/// Сколько сервер должен прожить, чтобы падение считалось единичным.
const STABLE_AFTER_SECS: u64 = 300;

/// Пауза перед подъёмом. Растёт с попытками — если причина не ушла, незачем
/// долбить докер раз в секунду.
fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs(5 * attempt.min(6) as u64)
}

/// Присоединиться к контейнеру и гнать его вывод в исходящую очередь.
///
/// Задача живёт, пока жив контейнер: когда докер закрывает поток, сервер
/// завершился — тогда и выясняется, вышел он сам, упал или был убит лимитом
/// памяти.
/// Возвращает упакованный future намеренно: присмотр поднимает упавший сервер и
/// снова зовёт `attach`, а рекурсивный `async fn` даёт бесконечный тип, который
/// компилятор к тому же не может признать `Send`.
pub fn attach(
    engine: Engine,
    registry: Registry,
    outbox: mpsc::Sender<NodeEvent>,
    server: Uuid,
) -> futures_util::future::BoxFuture<'static, Result<()>> {
    Box::pin(attach_inner(engine, registry, outbox, server))
}

async fn attach_inner(
    engine: Engine,
    registry: Registry,
    outbox: mpsc::Sender<NodeEvent>,
    server: Uuid,
) -> Result<()> {
    let (console_rx, writer) = console::attach(&engine, server).await?;
    let handle = registry.get_or_create(server);
    // Номер и ручка меняются под одним замком: присмотр прошлого запуска
    // сверяет номер под ним же, прежде чем снять ручку.
    let generation = {
        let mut slot = handle.writer.lock().await;
        *slot = Some(writer);
        handle.next_generation()
    };
    handle.set_power(PowerState::Starting);

    offer(&outbox, state_event(server, &handle, 0, None));

    tokio::spawn(pump(
        engine, registry, handle, outbox, console_rx, server, generation,
    ));
    Ok(())
}

/// Положить событие в очередь к мастеру, не дожидаясь места в ней.
///
/// Очередь разбирает только живое соединение. Пока мастера нет, она полна, и
/// ожидание места останавливало чтение консоли — а непрочитанный вывод докер
/// держит в трубе, пока процесс сервера не повиснет на записи в stdout. Сбой
/// мастера замораживал игру на всех серверах ноды. Возвращает `false`, если
/// событие пришлось выбросить.
fn offer(outbox: &mpsc::Sender<NodeEvent>, event: NodeEvent) -> bool {
    outbox.try_send(event).is_ok()
}

async fn pump(
    engine: Engine,
    registry: Registry,
    handle: Arc<ServerHandle>,
    outbox: mpsc::Sender<NodeEvent>,
    mut console_rx: mpsc::Receiver<ConsoleBatch>,
    server: Uuid,
    generation: u64,
) {
    // Строки, выброшенные из-за полной очереди, не пропадают бесследно: их
    // число уходит со следующей пачкой тем же `skipped`, что и при переполнении
    // кадра.
    let mut dropped: u32 = 0;
    while let Some(batch) = console_rx.recv().await {
        // К тому же контейнеру уже сел новый присмотр: второй поток слал бы
        // каждую строку дважды. Уход отсюда закрывает и поток докера.
        if handle.generation() != generation {
            return;
        }
        if !handle.state().ready && batch.lines.iter().any(|l| console::marks_ready(l)) {
            handle.set_ready();
            handle.set_power(PowerState::Running);
            // Состояние догонит периодический опрос, если сейчас не влезло.
            offer(&outbox, state_event(server, &handle, 0, None));
        }

        let lost = batch.lines.len() as u32 + batch.skipped;
        let event = NodeEvent::Console {
            server,
            lines: batch.lines,
            skipped: batch.skipped.saturating_add(dropped),
        };
        if offer(&outbox, event) {
            dropped = 0;
        } else {
            dropped = dropped.saturating_add(lost);
            crate::diag::DROPPED_CONSOLE_LINES
                .fetch_add(u64::from(lost), std::sync::atomic::Ordering::Relaxed);
        }
    }

    // Поток закрыт — контейнер больше не пишет. Если за это время сервер
    // успели поднять заново, всё ниже — забота нового присмотра: снять его
    // ручку stdin или доложить о «падении» значило бы соврать о живом.
    {
        let mut slot = handle.writer.lock().await;
        if handle.generation() != generation {
            return;
        }
        *slot = None;
    }

    // Чем это кончилось, знает докер.
    let (power, exit_code, uptime) =
        engine
            .state(server)
            .await
            .unwrap_or((PowerState::Offline, None, 0));
    handle.set_power(power);
    let stopped_by_user = handle.state().stopped_on_purpose;

    // Без ожидания и здесь: подъём упавшего сервера ниже не должен ждать,
    // пока вернётся мастер.
    offer(
        &outbox,
        NodeEvent::ServerState {
            server,
            state: power,
            ready: false,
            uptime_secs: uptime,
            exit_code,
            stopped_by_user,
        },
    );

    // Долгая работа до падения — причина случайная, и прошлые попытки не в счёт.
    if uptime >= STABLE_AFTER_SECS {
        handle.forget_restarts();
    }

    // Поднимаем только упавшее. Штатный выход — это `stop` изнутри игры или
    // команда человека, и воскрешать его было бы спором с тем, кто её дал.
    if matches!(power, PowerState::Crashed | PowerState::OomKilled) {
        try_restart(engine, registry, outbox, server, power, generation).await;
    }
}

/// Поднять упавший сервер, если попытки ещё остались.
async fn try_restart(
    engine: Engine,
    registry: Registry,
    outbox: mpsc::Sender<NodeEvent>,
    server: Uuid,
    power: PowerState,
    generation: u64,
) {
    let handle = registry.get_or_create(server);
    let Some(attempt) = handle.take_restart_slot(MAX_RESTARTS) else {
        tracing::warn!(%server, ?power, "автоподъём не делается: остановлен вручную или исчерпаны попытки");
        return;
    };

    let pause = backoff(attempt);
    tracing::info!(%server, ?power, attempt, secs = pause.as_secs(), "поднимаю упавший сервер");
    offer(
        &outbox,
        NodeEvent::Console {
            server,
            lines: vec![format!(
                "[noro] сервер упал ({power:?}), подъём через {} с (попытка {attempt} из {MAX_RESTARTS})",
                pause.as_secs()
            )],
            skipped: 0,
        },
    );
    tokio::time::sleep(pause).await;

    // Между падением и подъёмом человек мог погасить сервер сам или поднять
    // его заново — проверяем ещё раз.
    if handle.state().stopped_on_purpose || handle.generation() != generation {
        return;
    }

    match engine.start(server).await {
        Ok(()) => {
            if let Err(e) = attach(engine, registry, outbox, server).await {
                tracing::warn!(%server, error = %e, "поднял, но не сел на консоль");
            }
        }
        Err(e) => {
            tracing::warn!(%server, error = %e, "не поднять упавший сервер");
            offer(
                &outbox,
                NodeEvent::Console {
                    server,
                    lines: vec![format!("[noro] подъём не удался: {e}")],
                    skipped: 0,
                },
            );
        }
    }
}

fn state_event(
    server: Uuid,
    handle: &ServerHandle,
    uptime_secs: u64,
    exit_code: Option<i32>,
) -> NodeEvent {
    let state = handle.state();
    NodeEvent::ServerState {
        server,
        state: state.power,
        ready: state.ready,
        uptime_secs,
        exit_code,
        stopped_by_user: state.stopped_on_purpose,
    }
}
