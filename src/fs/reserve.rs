//! Запас свободного места на диске ноды, который не отдаётся ни одному серверу.
//!
//! Квота сервера ограничивает только его самого, а сумма квот на ноде обычно
//! больше диска — переподписка — и у серверов, заведённых до квот, лимита нет
//! вовсе. Когда диск ноды кончался, падали все серверы разом, и падали с битыми
//! мирами: Minecraft не переживает отказ записи посреди сохранения. Запас
//! держит последние гигабайты для того, что уже работает: заливки, распаковки
//! и скачивания в это место получают отказ, а сами серверы продолжают писать.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Как долго верить замеру свободного места. Перечисление дисков — системные
/// вызовы на каждый смонтированный диск, а проверка идёт на каждый кусок
/// заливки по SFTP.
const FRESH: Duration = Duration::from_secs(10);

pub struct Reserve {
    data_dir: PathBuf,
    bytes: u64,
    /// Последний замер: когда и сколько было свободно.
    free: parking_lot::Mutex<Option<(Instant, u64)>>,
}

impl Reserve {
    pub fn new(data_dir: PathBuf, reserve_mb: u64) -> Self {
        Self {
            data_dir,
            bytes: reserve_mb.saturating_mul(1024 * 1024),
            free: parking_lot::Mutex::new(None),
        }
    }

    /// Сколько ещё можно записать, не трогая запас.
    pub fn room(&self) -> u64 {
        self.free_bytes().saturating_sub(self.bytes)
    }

    pub fn fits(&self, extra: u64) -> Result<()> {
        if extra > self.room() {
            bail!(
                "на диске ноды кончается место: свободно {} МБ, из них {} МБ — запас ноды",
                self.free_bytes() / 1024 / 1024,
                self.bytes / 1024 / 1024
            );
        }
        Ok(())
    }

    /// Учесть записанное до следующего замера: иначе пачка заливок за десять
    /// секунд проходила бы проверку по одному и тому же числу.
    pub fn consume(&self, bytes: u64) {
        if let Some((_, free)) = self.free.lock().as_mut() {
            *free = free.saturating_sub(bytes);
        }
    }

    fn free_bytes(&self) -> u64 {
        let mut cached = self.free.lock();
        match *cached {
            Some((at, free)) if at.elapsed() < FRESH => free,
            _ => {
                let free = crate::metrics::free_disk_bytes(&self.data_dir);
                *cached = Some((Instant::now(), free));
                free
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_free(free: u64, reserve_mb: u64) -> Reserve {
        let reserve = Reserve::new(PathBuf::from("/nonexistent"), reserve_mb);
        *reserve.free.lock() = Some((Instant::now(), free));
        reserve
    }

    #[test]
    fn the_reserve_is_never_handed_out() {
        let mb = 1024 * 1024;
        let reserve = with_free(1500 * mb, 1024);
        assert!(reserve.fits(400 * mb).is_ok());
        assert!(reserve.fits(500 * mb).is_err());
        assert!(reserve.fits(u64::MAX).is_err());
    }

    #[test]
    fn writes_between_measurements_are_counted() {
        let mb = 1024 * 1024;
        let reserve = with_free(1500 * mb, 1024);
        reserve.consume(400 * mb);
        assert!(reserve.fits(100 * mb).is_err());
    }
}
