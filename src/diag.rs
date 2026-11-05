//! Состояние самого демона: связь с мастером, очередь событий, серверы.
//!
//! «Нода зависла?» прежде выяснялось по логу демона, а в нём нет главного:
//! жив ли канал с мастером сейчас, копится ли очередь, теряются ли строки
//! консоли и чем занят каждый сервер. Отчёт отдаётся мастеру в ответе на
//! `NodeInfo` и оператору машины — `curl localhost:<порт>/status`.

use schema::noded::{MasterLink, NodeDiag, ServerDiag, SftpDiag};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use crate::Daemon;

/// Строки консоли, выброшенные из-за полной очереди к мастеру.
pub static DROPPED_CONSOLE_LINES: AtomicU64 = AtomicU64::new(0);

pub struct Health {
    started: Instant,
    connected: AtomicBool,
    /// Когда от мастера в последний раз что-то пришло, unix-секунды.
    last_contact: AtomicI64,
    /// Сколько раз канал с мастером рвался с момента старта.
    disconnects: AtomicU64,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            connected: AtomicBool::new(false),
            last_contact: AtomicI64::new(0),
            disconnects: AtomicU64::new(0),
        }
    }
}

impl Health {
    pub fn connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        self.touch();
    }

    pub fn disconnected(&self) {
        if self.connected.swap(false, Ordering::Relaxed) {
            self.disconnects.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn touch(&self) {
        self.last_contact
            .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    }
}

/// Отчёт целиком. Только чтение памяти демона: докер и диск не трогаются,
/// чтобы ответ приходил и тогда, когда завис именно докер.
pub fn report(daemon: &Daemon) -> Value {
    serde_json::to_value(diag(daemon)).unwrap_or(Value::Null)
}

fn diag(daemon: &Daemon) -> NodeDiag {
    let health = &daemon.health;
    let last_contact = health.last_contact.load(Ordering::Relaxed);
    let busy: std::collections::HashMap<_, _> = daemon.busy.snapshot().into_iter().collect();

    let mut servers: Vec<ServerDiag> = daemon
        .registry
        .ids()
        .into_iter()
        .filter_map(|id| {
            let handle = daemon.registry.get(id)?;
            let state = handle.state();
            Some(ServerDiag {
                id,
                power: state.power,
                ready: state.ready,
                stopped_on_purpose: state.stopped_on_purpose,
                restarts: state.restarts,
                // Занятый замок значит, что в консоль как раз пишут; это не
                // «консоли нет».
                console: handle
                    .writer
                    .try_lock()
                    .map(|w| w.is_some())
                    .unwrap_or(true),
                busy: busy.get(&id).map(|what| what.to_string()),
                sleeping: daemon.placeholders.contains_key(&id),
            })
        })
        .collect();
    servers.sort_by_key(|s| s.id);

    NodeDiag {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: health.started.elapsed().as_secs(),
        master: MasterLink {
            connected: health.connected.load(Ordering::Relaxed),
            last_contact: (last_contact > 0).then_some(last_contact),
            disconnects: health.disconnects.load(Ordering::Relaxed),
        },
        events_queued: (daemon.events.max_capacity() - daemon.events.capacity()) as u64,
        console_lines_dropped: DROPPED_CONSOLE_LINES.load(Ordering::Relaxed),
        sftp: SftpDiag {
            enabled: daemon.cfg.sftp.enabled,
            fingerprint: daemon.sftp_fingerprint.lock().clone(),
        },
        servers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnect_is_counted_once() {
        let health = Health::default();
        health.connected();
        health.disconnected();
        health.disconnected();
        assert_eq!(health.disconnects.load(Ordering::Relaxed), 1);
        assert!(health.last_contact.load(Ordering::Relaxed) > 0);
    }
}
