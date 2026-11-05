// Файл превышает 150 строк: замер, кэш и проверка лимита — одна механика, и порознь она не читается.
//! Сколько места занимает сервер и можно ли писать ещё.
//!
//! Докеровский `--storage-opt size=` не годится: он работает только на overlay2
//! поверх xfs с pquota, то есть на меньшинстве машин. Поэтому место меряется
//! обходом каталога, а лимит применяется нами — зато одинаково везде.
//!
//! Обход каталога с миром стоит секунды, поэтому он кэшируется, а записи между
//! замерами добавляются к последнему числу. Кэш врёт в безопасную сторону:
//! удаления он не замечает до следующего замера, так что отказ может прийти
//! чуть раньше, чем место реально кончится, но не позже.
//!
//! Сам обход уходит в блокирующий пул. Секунды хождения по сотне тысяч файлов
//! на рабочих потоках рантайма — это замершая консоль у всех серверов ноды
//! разом, и замер, который делается раз в двадцать секунд на каждый сервер,
//! устраивал это регулярно.

use anyhow::{bail, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Через сколько замер считается несвежим.
const TTL: Duration = Duration::from_secs(60);

/// Прежнее место лимитов — внутри служебного каталога сервера. Оттуда они
/// только переносятся: каталог примонтирован в контейнер, и процесс сервера
/// переписывал `{"disk_mb": 0}` — то есть снимал с себя квоту.
const LEGACY_LIMITS_FILE: &str = "limits.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Limits {
    /// Ноль — лимита нет: так ведут себя серверы, заведённые до появления квоты.
    pub disk_mb: i64,
}

/// Где лежат лимиты: рядом с каталогом сервера, а не в нём.
///
/// На диске, а не в памяти: лимит нужен и после перезапуска демона, а реестр
/// переживать его не должен — правда о контейнерах живёт в докере. Рядом, а не
/// внутри: в контейнер монтируется только сам каталог сервера.
pub fn limits_path(root: &Path) -> std::path::PathBuf {
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push(".limits.json");
    root.with_file_name(name)
}

pub fn write_limits(root: &Path, limits: &Limits) -> Result<()> {
    crate::fs::nofollow::write(&limits_path(root), &serde_json::to_vec_pretty(limits)?)?;
    let legacy = root
        .join(crate::server::layout::SERVICE_DIR)
        .join(LEGACY_LIMITS_FILE);
    let _ = crate::fs::nofollow::remove(&legacy);
    Ok(())
}

pub fn read_limits(root: &Path) -> Limits {
    if let Ok(bytes) = std::fs::read(limits_path(root)) {
        return serde_json::from_slice(&bytes).unwrap_or_default();
    }
    // Сервер ставили до переноса: берём старый файл один раз и переносим.
    // Подделать его владелец мог и раньше; после переноса — уже нет.
    let legacy = root
        .join(crate::server::layout::SERVICE_DIR)
        .join(LEGACY_LIMITS_FILE);
    let Some(limits) = crate::fs::nofollow::read(&legacy)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Limits>(&bytes).ok())
    else {
        return Limits::default();
    };
    if let Err(e) = write_limits(root, &limits) {
        tracing::warn!(error = %e, "лимит диска не перенесён");
    }
    limits
}

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub used_bytes: u64,
    /// Ноль — лимита нет.
    pub limit_bytes: u64,
}

impl Usage {
    pub fn used_mb(&self) -> i64 {
        (self.used_bytes / 1024 / 1024) as i64
    }

    /// Доля занятого, 0.0…1.0+. Без лимита — ноль: делить не на что.
    pub fn ratio(&self) -> f64 {
        if self.limit_bytes == 0 {
            return 0.0;
        }
        self.used_bytes as f64 / self.limit_bytes as f64
    }

    /// Влезет ли ещё `extra` байт. Отдельно от `Quota`, чтобы длинная заливка
    /// сверялась с однажды снятым замером, а не ходила по каталогу на каждый
    /// пришедший кусок.
    pub fn fits(&self, extra: u64) -> Result<()> {
        if self.limit_bytes == 0 {
            return Ok(());
        }
        // Насыщение, а не сложение: `extra` бывает и из заголовка запроса, и
        // `Content-Length: 18446744073709551615` переполнял сумму в малое число —
        // заливка без предела проходила проверку.
        if self.used_bytes.saturating_add(extra) > self.limit_bytes {
            bail!(
                "не хватает места: занято {} МБ из {} МБ, требуется ещё {} МБ",
                self.used_bytes / 1024 / 1024,
                self.limit_bytes / 1024 / 1024,
                extra.div_ceil(1024 * 1024)
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Entry {
    used_bytes: u64,
    limit_bytes: u64,
    measured_at: Instant,
}

#[derive(Clone, Default)]
pub struct Quota {
    inner: Arc<DashMap<Uuid, Entry>>,
    /// Запас диска ноды поверх квот серверов. `None` — запаса нет.
    reserve: Option<Arc<super::reserve::Reserve>>,
}

impl Quota {
    pub fn with_reserve(reserve: super::reserve::Reserve) -> Self {
        Self {
            inner: Arc::default(),
            reserve: Some(Arc::new(reserve)),
        }
    }

    /// Сколько ещё можно записать на ноду, не трогая её запас.
    pub fn node_room(&self) -> Option<u64> {
        self.reserve.as_ref().map(|r| r.room())
    }

    /// Влезет ли `extra` байт в диск ноды за вычетом запаса.
    pub fn node_fits(&self, extra: u64) -> Result<()> {
        match &self.reserve {
            Some(reserve) => reserve.fits(extra),
            None => Ok(()),
        }
    }

    /// Текущее занятое место. Меряет заново, если последний замер протух.
    pub async fn usage(&self, server: Uuid, root: &Path) -> Usage {
        if let Some(entry) = self.cached(server) {
            return entry;
        }
        self.measure(server, root).await
    }

    fn cached(&self, server: Uuid) -> Option<Usage> {
        let entry = self.inner.get(&server)?;
        (entry.measured_at.elapsed() < TTL).then_some(Usage {
            used_bytes: entry.used_bytes,
            limit_bytes: entry.limit_bytes,
        })
    }

    /// Пересчитать с нуля. Вызывается после удаления файлов, когда ждать
    /// протухания кэша незачем.
    pub async fn measure(&self, server: Uuid, root: &Path) -> Usage {
        let path = root.to_path_buf();
        let measured = tokio::task::spawn_blocking(move || {
            (dir_size(&path), read_limits(&path).disk_mb.max(0) as u64)
        })
        .await;

        let Ok((used_bytes, limit_mb)) = measured else {
            // Задача обхода не доехала. Прежнее число честнее нуля: с нулём
            // сервер, у которого место кончилось, снова начал бы принимать
            // заливки.
            tracing::warn!(%server, "замер занятого места не выполнен");
            return self.cached(server).unwrap_or(Usage {
                used_bytes: 0,
                limit_bytes: 0,
            });
        };

        let limit_bytes = limit_mb.saturating_mul(1024 * 1024);
        self.inner.insert(
            server,
            Entry {
                used_bytes,
                limit_bytes,
                measured_at: Instant::now(),
            },
        );
        Usage {
            used_bytes,
            limit_bytes,
        }
    }

    /// Хватит ли места ещё на `extra` байт.
    ///
    /// Без лимита разрешает всё: это не «безлимит по недосмотру», а сервер,
    /// заведённый до появления квоты, и отказывать ему задним числом нельзя.
    pub async fn check(&self, server: Uuid, root: &Path, extra: u64) -> Result<()> {
        self.usage(server, root).await.fits(extra)?;
        self.node_fits(extra)
    }

    /// Учесть записанное, не пересчитывая каталог заново.
    pub fn add(&self, server: Uuid, bytes: u64) {
        if let Some(mut entry) = self.inner.get_mut(&server) {
            entry.used_bytes = entry.used_bytes.saturating_add(bytes);
        }
        if let Some(reserve) = &self.reserve {
            reserve.consume(bytes);
        }
    }

    pub fn forget(&self, server: Uuid) {
        self.inner.remove(&server);
    }
}

/// Размер каталога по содержимому файлов.
///
/// Симлинки не разворачиваются: файл, на который они указывают, либо уже
/// посчитан внутри каталога, либо лежит снаружи и к серверу не относится.
pub fn dir_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
