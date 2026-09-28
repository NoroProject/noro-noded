//! Метрики контейнера из докера.

use anyhow::Result;
use bollard::models::ContainerStatsResponse;
use bollard::query_parameters::StatsOptionsBuilder;
use futures_util::StreamExt;
use schema::noded::ServerStats;

use super::engine::Engine;

impl Engine {
    /// Один замер. Стрим докера мы не держим постоянно: он присылает данные раз
    /// в секунду на каждый контейнер, а панели хватает заметно более редкого
    /// опроса.
    pub async fn stats_once(&self, server: uuid::Uuid) -> Result<ServerStats> {
        let options = StatsOptionsBuilder::default().stream(false).build();
        let mut stream = self
            .docker
            .stats(&Engine::container_name(server), Some(options));

        let Some(sample) = stream.next().await else {
            return Ok(ServerStats::default());
        };
        Ok(convert(sample?))
    }

    /// То же, но с занятым местом.
    ///
    /// Диск докер в статистике не отдаёт вовсе — он не знает про наш маунт, —
    /// поэтому число приходит от квоты, которая каталог и меряет.
    pub async fn stats_with_disk(
        &self,
        server: uuid::Uuid,
        quota: &crate::fs::quota::Quota,
        root: &std::path::Path,
    ) -> Result<ServerStats> {
        let mut stats = self.stats_once(server).await?;
        stats.disk_mb = quota.usage(server, root).await.used_mb();
        Ok(stats)
    }
}

fn convert(s: ContainerStatsResponse) -> ServerStats {
    let memory_mb = s
        .memory_stats
        .as_ref()
        .and_then(|m| m.usage)
        .map(|b| (b / 1024 / 1024) as i64)
        .unwrap_or(0);

    let memory_limit_mb = s
        .memory_stats
        .as_ref()
        .and_then(|m| m.limit)
        .map(|b| (b / 1024 / 1024) as i64)
        .unwrap_or(0);

    let (rx, tx) = s
        .networks
        .as_ref()
        .map(|nets| {
            nets.values().fold((0u64, 0u64), |(rx, tx), n| {
                (rx + n.rx_bytes.unwrap_or(0), tx + n.tx_bytes.unwrap_or(0))
            })
        })
        .unwrap_or((0, 0));

    ServerStats {
        cpu_percent: cpu_percent(&s),
        memory_mb,
        memory_limit_mb,
        disk_mb: 0,
        net_rx_bytes: rx,
        net_tx_bytes: tx,
    }
}

/// Загрузка процессора в процентах от одного ядра.
///
/// Докер отдаёт счётчики, а не проценты: доля считается от **разницы** с
/// предыдущим замером, который он же и присылает в том же ответе. Без деления
/// на разницу системного времени получались бы «проценты» в тысячах.
fn cpu_percent(s: &ContainerStatsResponse) -> f64 {
    let Some(cpu) = s.cpu_stats.as_ref() else {
        return 0.0;
    };
    let Some(pre) = s.precpu_stats.as_ref() else {
        return 0.0;
    };

    let used = cpu
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let used_before = pre
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let system = cpu.system_cpu_usage.unwrap_or(0);
    let system_before = pre.system_cpu_usage.unwrap_or(0);

    let delta = used.saturating_sub(used_before) as f64;
    let system_delta = system.saturating_sub(system_before) as f64;
    if system_delta <= 0.0 || delta <= 0.0 {
        return 0.0;
    }

    // online_cpus отсутствует на старых демонах — тогда считаем по одному ядру,
    // и цифра остаётся честной для «процентов одного ядра».
    let cores = cpu.online_cpus.unwrap_or(1).max(1) as f64;
    (delta / system_delta) * cores * 100.0
}
