//! Что демон помнит о серверах между запросами.
//!
//! Только оперативное: ручка консоли, состояние питания, признак готовности.
//! Ничего из этого не переживает перезапуск демона и не должно — правда о
//! контейнерах живёт в докере, и после старта она пересчитывается заново.

use dashmap::DashMap;
use schema::noded::PowerState;
use std::sync::Arc;
use uuid::Uuid;

use crate::docker::console::ConsoleWriter;

#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeState {
    pub power: PowerState,
    /// Сервер отпечатал `Done (` в этом запуске.
    pub ready: bool,
    /// Сервер погасили по команде, а не он упал сам.
    ///
    /// Без этого флага автоподъём воскрешал бы то, что человек только что
    /// остановил — включая приостановленный сервер, который мастер гасит той же
    /// командой.
    pub stopped_on_purpose: bool,
    /// Сколько раз подряд поднимали после падения.
    pub restarts: u32,
}

pub struct ServerHandle {
    /// Ручка stdin. Пока она жива, в сервер можно слать команды; уронить её —
    /// значит закрыть серверу ввод, поэтому она снимается только вместе с
    /// остановкой.
    pub writer: tokio::sync::Mutex<Option<ConsoleWriter>>,
    state: parking_lot::Mutex<RuntimeState>,
}

impl ServerHandle {
    fn new() -> Self {
        Self {
            writer: tokio::sync::Mutex::new(None),
            state: parking_lot::Mutex::new(RuntimeState::default()),
        }
    }

    pub fn state(&self) -> RuntimeState {
        *self.state.lock()
    }

    pub fn set_power(&self, power: PowerState) {
        let mut state = self.state.lock();
        state.power = power;
        // Готовность живёт ровно один запуск: после остановки сервер снова
        // «не готов», даже если контейнер тот же.
        if !power.is_up() {
            state.ready = false;
        }
    }

    pub fn set_ready(&self) {
        self.state.lock().ready = true;
    }

    /// Пометить, что остановка — намеренная, и поднимать сервер не надо.
    pub fn stopping_on_purpose(&self) {
        let mut state = self.state.lock();
        state.stopped_on_purpose = true;
        state.restarts = 0;
    }

    /// Запуск по команде: счётчик падений обнуляется, флаг снимается.
    pub fn starting_on_purpose(&self) {
        let mut state = self.state.lock();
        state.stopped_on_purpose = false;
        state.restarts = 0;
    }

    /// Взять разрешение на автоподъём. `None` — поднимать не будем.
    pub fn take_restart_slot(&self, max: u32) -> Option<u32> {
        let mut state = self.state.lock();
        if state.stopped_on_purpose || state.restarts >= max {
            return None;
        }
        state.restarts += 1;
        Some(state.restarts)
    }

    /// Сервер прожил достаточно долго — прошлые падения больше не в счёт.
    pub fn forget_restarts(&self) {
        self.state.lock().restarts = 0;
    }
}

#[derive(Default, Clone)]
pub struct Registry {
    servers: Arc<DashMap<Uuid, Arc<ServerHandle>>>,
}

impl Registry {
    pub fn get_or_create(&self, server: Uuid) -> Arc<ServerHandle> {
        self.servers
            .entry(server)
            .or_insert_with(|| Arc::new(ServerHandle::new()))
            .clone()
    }

    pub fn get(&self, server: Uuid) -> Option<Arc<ServerHandle>> {
        self.servers.get(&server).map(|h| h.clone())
    }

    pub fn forget(&self, server: Uuid) {
        self.servers.remove(&server);
    }

    pub fn ids(&self) -> Vec<Uuid> {
        self.servers.iter().map(|e| *e.key()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_does_not_survive_a_stop() {
        let registry = Registry::default();
        let handle = registry.get_or_create(Uuid::nil());

        handle.set_power(PowerState::Running);
        handle.set_ready();
        assert!(handle.state().ready);

        handle.set_power(PowerState::Offline);
        assert!(
            !handle.state().ready,
            "остановленный сервер не может оставаться готовым"
        );
    }

    const MAX: u32 = 3;

    #[test]
    fn a_crashed_server_gets_its_attempts_and_then_stops() {
        let registry = Registry::default();
        let handle = registry.get_or_create(Uuid::nil());

        assert_eq!(handle.take_restart_slot(MAX), Some(1));
        assert_eq!(handle.take_restart_slot(MAX), Some(2));
        assert_eq!(handle.take_restart_slot(MAX), Some(3));
        assert_eq!(
            handle.take_restart_slot(MAX),
            None,
            "падающий по кругу сервер перестают поднимать"
        );
    }

    /// Демон не спорит с человеком: погашенный вручную сервер не воскресает.
    #[test]
    fn a_server_stopped_on_purpose_is_never_revived() {
        let registry = Registry::default();
        let handle = registry.get_or_create(Uuid::nil());

        handle.stopping_on_purpose();

        assert_eq!(handle.take_restart_slot(MAX), None);
    }

    #[test]
    fn starting_again_clears_the_refusal() {
        let registry = Registry::default();
        let handle = registry.get_or_create(Uuid::nil());

        handle.stopping_on_purpose();
        handle.starting_on_purpose();

        assert_eq!(handle.take_restart_slot(MAX), Some(1));
    }

    /// Два падения с разницей в неделю — это не «падает по кругу».
    #[test]
    fn a_long_uptime_forgives_past_crashes() {
        let registry = Registry::default();
        let handle = registry.get_or_create(Uuid::nil());

        handle.take_restart_slot(MAX);
        handle.take_restart_slot(MAX);
        handle.forget_restarts();

        assert_eq!(handle.take_restart_slot(MAX), Some(1));
    }

    #[test]
    fn the_same_server_gets_the_same_handle() {
        let registry = Registry::default();
        let a = registry.get_or_create(Uuid::nil());
        let b = registry.get_or_create(Uuid::nil());
        assert!(Arc::ptr_eq(&a, &b));
    }
}
