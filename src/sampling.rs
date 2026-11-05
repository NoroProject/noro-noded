//! Периодические замеры: серверы раз в двадцать секунд, машина — в полминуты,
//! и предупреждения о месте, которые из этих замеров следуют.

use schema::noded::{NodeEvent, PressureKind};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{fs, metrics, Daemon};

/// Замеры по каждому работающему серверу.
///
/// Без этой задачи `NodeEvent::Stats` не отправлялся вовсе: мастер его ждал, а
/// графики в панели рисовали пустоту. Раз в двадцать секунд — чаще незачем,
/// докер отдаёт счётчики с секундным шагом, а обход каталога под диск не
/// бесплатен.
pub fn spawn_server_stats(daemon: Daemon, events: mpsc::Sender<NodeEvent>) {
    use futures_util::StreamExt;

    tokio::spawn(async move {
        // Серверы, о чьей квоте мастер уже предупреждён.
        let mut raised = std::collections::HashSet::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
        // Докер отвечает на замер не сразу, а через секунду-две: ему нужна
        // разница счётчиков. По одному серверу за раз двадцать серверов не
        // укладывались в интервал, а пропущенные тики `interval` по умолчанию
        // догоняет залпом — и опрос докера шёл без передышки. Теперь замеры
        // идут по нескольку разом, а опоздавший тик просто сдвигается.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let up: Vec<_> = daemon
                .registry
                .ids()
                .into_iter()
                .filter(|server| {
                    daemon
                        .registry
                        .get(*server)
                        .is_some_and(|h| h.state().power.is_up())
                })
                .collect();
            let samples = futures_util::stream::iter(up)
                .map(|server| {
                    let daemon = &daemon;
                    async move {
                        let root = daemon.cfg.server_dir(server);
                        let stats = daemon
                            .engine
                            .stats_with_disk(server, &daemon.quota, &root)
                            .await;
                        (server, stats)
                    }
                })
                .buffer_unordered(STATS_PARALLEL)
                .collect::<Vec<_>>()
                .await;
            for (server, stats) in samples {
                match stats {
                    Ok(stats) => {
                        let _ = events.try_send(NodeEvent::Stats { server, stats });
                        let usage = daemon
                            .quota
                            .usage(server, &daemon.cfg.server_dir(server))
                            .await;
                        warn_server_disk(&events, &mut raised, server, usage);
                    }
                    // Контейнер могли снять между перечислением и замером —
                    // это не повод шуметь в лог на каждом тике.
                    Err(e) => tracing::debug!(%server, error = %e, "замер не удался"),
                }
            }
        }
    });
}

/// Сказать мастеру, что сервер подошёл к своей квоте или отошёл от неё.
///
/// Не доехавшее до очереди не считается сказанным: иначе предупреждение,
/// выброшенное, пока мастера не было, не повторилось бы никогда.
fn warn_server_disk(
    events: &mpsc::Sender<NodeEvent>,
    raised: &mut std::collections::HashSet<Uuid>,
    server: Uuid,
    usage: fs::quota::Usage,
) {
    let was = raised.contains(&server);
    let Some(now) = fs::pressure::step(was, usage.used_bytes, usage.limit_bytes) else {
        return;
    };
    let sent = events
        .try_send(NodeEvent::DiskPressure {
            kind: PressureKind::ServerDisk,
            server: Some(server),
            used_mb: usage.used_mb(),
            limit_mb: (usage.limit_bytes / 1024 / 1024) as i64,
            active: now,
        })
        .is_ok();
    if sent {
        if now {
            raised.insert(server);
        } else {
            raised.remove(&server);
        }
    }
}

/// Сколько серверов меряется одновременно.
const STATS_PARALLEL: usize = 8;

/// Метрики машины раз в полминуты: чаще панели не нужно, а sysinfo не бесплатен.
pub fn spawn_node_stats(daemon: Daemon, events: mpsc::Sender<NodeEvent>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sampler = metrics::Sampler::new();
        let mut raised = false;
        loop {
            ticker.tick().await;
            let running = daemon
                .registry
                .ids()
                .iter()
                .filter(|id| {
                    daemon
                        .registry
                        .get(**id)
                        .map(|h| h.state().power.is_up())
                        .unwrap_or(false)
                })
                .count() as u32;
            let stats = sampler.sample(running, &daemon.cfg.data_dir);
            let _ = events.send(NodeEvent::NodeStats { stats }).await;

            // Запас ноды — последнее, что держит живые серверы: о его исходе
            // персонал должен знать раньше, чем начнутся отказы.
            let (used, limit) = fs::pressure::node_usage(
                stats.disk_used_mb,
                stats.disk_total_mb,
                daemon.cfg.disk_reserve_mb,
            );
            if let Some(now) = fs::pressure::step(raised, used, limit) {
                let event = NodeEvent::DiskPressure {
                    kind: PressureKind::NodeDisk,
                    server: None,
                    used_mb: used as i64,
                    limit_mb: limit as i64,
                    active: now,
                };
                if events.try_send(event).is_ok() {
                    raised = now;
                }
            }
        }
    });
}
