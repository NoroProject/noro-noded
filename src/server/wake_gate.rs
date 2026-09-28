//! Ограничитель побудок спящего сервера.
//!
//! Стук на порт приходит из интернета: без имени, без авторизации и сколько
//! угодно раз — логин-пакет идёт до аутентификации, и иначе быть не может.
//!
//! Ограничитель делает две вещи одной проверкой. Не даёт раскачивать один и тот
//! же сервер чаще интервала: неудачный запуск в паре с пересозданием плашки
//! мастером иначе складывается в цикл. И служит защёлкой на время проверки
//! допуска — та асинхронная, плашка на порту всё ещё отвечает, и пачка стуков
//! без отметки времени породила бы пачку запусков одного контейнера.

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Чаще смысла нет: разбуженный сервер всё равно живёт до засыпания по простою,
/// а до истечения этого интервала повторный стук ничего нового не сообщает.
const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct WakeGate {
    last: Arc<DashMap<Uuid, Instant>>,
    cooldown: Duration,
}

impl Default for WakeGate {
    fn default() -> Self {
        Self::with_cooldown(COOLDOWN)
    }
}

impl WakeGate {
    pub fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            last: Arc::new(DashMap::new()),
            cooldown,
        }
    }

    /// Пропустить побудку, отметив время, либо отказать.
    ///
    /// Отметка ставится в той же операции, что и проверка: между «посмотрели» и
    /// «записали» успевает пройти второй стук, и тогда защёлка не защёлка.
    pub fn allow(&self, server: Uuid) -> bool {
        let now = Instant::now();
        match self.last.entry(server) {
            dashmap::mapref::entry::Entry::Occupied(mut slot) => {
                if now.duration_since(*slot.get()) < self.cooldown {
                    return false;
                }
                slot.insert(now);
                true
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_knock_in_a_row_is_refused() {
        let gate = WakeGate::default();
        let server = Uuid::new_v4();
        assert!(gate.allow(server), "первый стук должен пройти");
        assert!(!gate.allow(server), "второй подряд — раскачка");
    }

    #[test]
    fn a_different_server_is_not_affected() {
        let gate = WakeGate::default();
        assert!(gate.allow(Uuid::new_v4()));
        assert!(
            gate.allow(Uuid::new_v4()),
            "ограничение на сервер, не общее"
        );
    }

    #[test]
    fn nothing_is_refused_once_the_interval_has_passed() {
        let gate = WakeGate::with_cooldown(Duration::ZERO);
        let server = Uuid::new_v4();
        assert!(gate.allow(server));
        assert!(gate.allow(server), "интервал вышел — стук снова законный");
    }
}
