//! Метрики самой машины.

use schema::noded::NodeStats;
use sysinfo::{Disks, System};

pub struct Machine {
    pub cpus: u32,
    pub memory_mb: i64,
    pub disk_mb: i64,
}

/// Что за машина. Спрашивается один раз при подключении: числа эти не меняются
/// без перезагрузки, а System::new_all стоит заметно дороже обычного замера.
pub fn machine() -> Machine {
    let mut sys = System::new_all();
    sys.refresh_memory();
    let disks = Disks::new_with_refreshed_list();

    Machine {
        cpus: sys.cpus().len() as u32,
        memory_mb: (sys.total_memory() / 1024 / 1024) as i64,
        disk_mb: disks
            .list()
            .iter()
            .map(|d| (d.total_space() / 1024 / 1024) as i64)
            .max()
            .unwrap_or(0),
    }
}

/// Текущая загрузка ноды.
pub fn sample(running: u32) -> NodeStats {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_usage();

    let disks = Disks::new_with_refreshed_list();
    let (total, available) = disks
        .list()
        .iter()
        .map(|d| (d.total_space(), d.available_space()))
        .max_by_key(|(total, _)| *total)
        .unwrap_or((0, 0));

    NodeStats {
        cpu_percent: sys.global_cpu_usage() as f64,
        memory_used_mb: ((sys.total_memory() - sys.available_memory()) / 1024 / 1024) as i64,
        memory_total_mb: (sys.total_memory() / 1024 / 1024) as i64,
        disk_used_mb: ((total - available) / 1024 / 1024) as i64,
        disk_total_mb: (total / 1024 / 1024) as i64,
        servers_running: running,
    }
}
