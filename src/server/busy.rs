//! Одна тяжёлая операция на сервер за раз.
//!
//! Мастер шлёт операции независимо, и два щелчка подряд, бэкап по расписанию
//! поверх ручного или восстановление посреди установки приходили на ноду
//! одновременно. Два бэкапа отпускали `save-on` друг другу посреди упаковки,
//! восстановление распаковывало мир под установщиком, удаление сносило каталог
//! из-под синка. Вторая операция теперь получает отказ сразу и с причиной —
//! ждать её незачем: мастер перестаёт ждать раньше, чем кончится первая.

use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct Busy {
    running: Arc<DashMap<Uuid, &'static str>>,
}

/// Пока жив, сервер занят. Снимается и при панике задачи — `Drop` при
/// раскрутке стека отрабатывает тоже.
pub struct Guard {
    busy: Busy,
    server: Uuid,
}

impl Busy {
    /// Занять сервер под операцию `what`. Отказ несёт то, что уже идёт.
    pub fn try_take(&self, server: Uuid, what: &'static str) -> Result<Guard, &'static str> {
        match self.running.entry(server) {
            dashmap::mapref::entry::Entry::Occupied(slot) => Err(*slot.get()),
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(what);
                Ok(Guard {
                    busy: self.clone(),
                    server,
                })
            }
        }
    }

    /// Что сейчас идёт на серверах ноды — для диагностики.
    pub fn snapshot(&self) -> Vec<(Uuid, &'static str)> {
        self.running
            .iter()
            .map(|e| (*e.key(), *e.value()))
            .collect()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.busy.running.remove(&self.server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_heavy_operation_is_refused_with_the_reason() {
        let busy = Busy::default();
        let server = Uuid::new_v4();
        let guard = busy.try_take(server, "бэкап").unwrap();
        assert_eq!(busy.try_take(server, "восстановление").err(), Some("бэкап"));
        assert!(
            busy.try_take(Uuid::new_v4(), "бэкап").is_ok(),
            "другой сервер свободен"
        );
        drop(guard);
        assert!(busy.try_take(server, "восстановление").is_ok());
    }
}
