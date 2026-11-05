//! Метрики самой машины.

use schema::noded::NodeStats;
use std::path::Path;
use sysinfo::{Disks, System};

pub struct Machine {
    pub cpus: u32,
    pub memory_mb: i64,
    pub disk_mb: i64,
}

/// Что за машина. Спрашивается один раз при подключении: числа эти не меняются
/// без перезагрузки, а System::new_all стоит заметно дороже обычного замера.
pub fn machine(data_dir: &Path) -> Machine {
    let mut sys = System::new_all();
    sys.refresh_memory();
    let disks = Disks::new_with_refreshed_list();

    Machine {
        cpus: sys.cpus().len() as u32,
        memory_mb: (sys.total_memory() / 1024 / 1024) as i64,
        disk_mb: (data_disk(&disks, data_dir).0 / 1024 / 1024) as i64,
    }
}

/// Сколько памяти на машине свободно сейчас.
///
/// Отдельно от `sample`: побудке нужно одно число, а диски и загрузка CPU там
/// стоят дороже самого ответа, и спрашивают их раз в полминуты, а не на стук.
pub fn available_memory_mb() -> i64 {
    let mut sys = System::new();
    sys.refresh_memory();
    (sys.available_memory() / 1024 / 1024) as i64
}

/// Сколько байт свободно на диске с каталогом данных.
pub fn free_disk_bytes(data_dir: &Path) -> u64 {
    data_disk(&Disks::new_with_refreshed_list(), data_dir).1
}

/// Замеры загрузки ноды.
///
/// Загрузка процессора — разница двух замеров, и `System` обязан жить между
/// ними: созданный заново на каждый замер, он отдавал ноль, и график CPU ноды
/// в панели был плоским всегда.
pub struct Sampler {
    sys: System,
}

impl Sampler {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        Self { sys }
    }

    /// Текущая загрузка ноды.
    pub fn sample(&mut self, running: u32, data_dir: &Path) -> NodeStats {
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();

        let disks = Disks::new_with_refreshed_list();
        let (total, available) = data_disk(&disks, data_dir);
        let memory_total = self.sys.total_memory();

        NodeStats {
            cpu_percent: self.sys.global_cpu_usage() as f64,
            memory_used_mb: (memory_total.saturating_sub(self.sys.available_memory()) / 1024 / 1024)
                as i64,
            memory_total_mb: (memory_total / 1024 / 1024) as i64,
            disk_used_mb: (total.saturating_sub(available) / 1024 / 1024) as i64,
            disk_total_mb: (total / 1024 / 1024) as i64,
            servers_running: running,
        }
    }
}

/// Объём и свободное место того диска, на котором лежат серверы.
///
/// Прежде брался самый большой диск машины: на ноде с отдельным диском под
/// архивы панель показывала его, а место кончалось на системном, где и лежат
/// миры. Диск ищется по самой длинной точке монтирования, с которой начинается
/// каталог данных; не нашёлся — самый большой, как раньше.
fn data_disk(disks: &Disks, data_dir: &Path) -> (u64, u64) {
    let pick = |d: &sysinfo::Disk| (d.total_space(), d.available_space());
    disks
        .list()
        .iter()
        .filter(|d| data_dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .or_else(|| disks.list().iter().max_by_key(|d| d.total_space()))
        .map(pick)
        .unwrap_or((0, 0))
}
