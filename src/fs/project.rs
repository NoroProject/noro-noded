// Файл превышает 150 строк: распознавание project quota, выбор номера проекта и его назначение серверу — одна механика.
//! Квота диска средствами файловой системы — project quota на xfs и ext4.
//!
//! Каждому серверу достаётся номер проекта: им помечается его каталог с флагом
//! наследования, и всё, что в нём создаётся, считается в этот проект. Лимит
//! ставится ядру, и отказ в записи получает любой пишущий — в том числе сам
//! процесс сервера, которому обход каталога демоном не мешал ничем. Занятое
//! место ядро тоже знает само: замер — один системный вызов вместо обхода
//! сотни тысяч файлов.
//!
//! Нет поддержки на томе — остаётся обход каталога (`quota.rs`), как раньше.
//!
//! Чего это не закрывает: процесс в контейнере без `userns-remap` живёт в
//! начальном пространстве имён пользователей, и владелец файла может сам
//! перевесить его в другой проект ioctl'ом. Закрывает это `userns-remap`.

use schema::noded::DiskQuotaDiag;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// Номера ниже оставлены людям: `/etc/projects` админа машины начинается с
/// единиц и десятков.
const FIRST_ID: u32 = 100_000;
const ID_SPAN: u32 = 1 << 30;

/// Что делает ядро. За трейтом — чтобы порядок шагов проверялся без root.
pub trait ProjectFs: Send + Sync {
    /// Пометить каталог и всё в нём номером проекта.
    fn assign(&self, root: &Path, id: u32) -> std::io::Result<()>;
    /// Жёсткий лимит в байтах; ноль — без лимита.
    fn set_limit(&self, id: u32, bytes: u64) -> std::io::Result<()>;
    /// Занято байт в проекте.
    fn usage(&self, id: u32) -> std::io::Result<u64>;
    /// В проекте что-то лежит — значит, номер занят чужим.
    fn in_use(&self, id: u32) -> std::io::Result<bool>;
}

/// Том, на котором лежит каталог, — строка `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
    pub options: Vec<String>,
}

/// Найти том каталога `dir` в тексте `/proc/self/mountinfo`.
///
/// Берётся самая длинная точка монтирования, лежащая на пути к каталогу, а из
/// одинаковых — последняя: она смонтирована поверх прежних.
pub fn mount_of(mountinfo: &str, dir: &Path) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        if fields.len() < dash + 4 || dash < 6 {
            continue;
        }
        let point = PathBuf::from(unescape(fields[4]));
        if !dir.starts_with(&point) {
            continue;
        }
        let deeper = best
            .as_ref()
            .is_none_or(|b| point.components().count() >= b.point.components().count());
        if deeper {
            let options = fields[5]
                .split(',')
                .chain(fields[dash + 3].split(','))
                .map(str::to_string)
                .collect();
            best = Some(Mount {
                point,
                fstype: fields[dash + 1].to_string(),
                options,
            });
        }
    }
    best
}

/// `\040` и прочие восьмеричные коды, которыми ядро пишет пробелы в путях.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let code = bytes.get(i + 1..i + 4).and_then(|oct| {
            let text = std::str::from_utf8(oct).ok()?;
            u8::from_str_radix(text, 8).ok()
        });
        match (bytes[i], code) {
            (b'\\', Some(byte)) => {
                out.push(byte);
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Включена ли на томе project quota с принуждением. `Err` — почему нет,
/// словами для оператора.
pub fn supports(mount: &Mount) -> Result<(), String> {
    if !matches!(mount.fstype.as_str(), "xfs" | "ext4") {
        return Err(format!(
            "{} on {} has no project quota",
            mount.fstype,
            mount.point.display()
        ));
    }
    let enforced = mount
        .options
        .iter()
        .any(|o| o == "prjquota" || o == "pquota");
    if !enforced {
        return Err(format!(
            "{} ({}) is mounted without prjquota",
            mount.point.display(),
            mount.fstype
        ));
    }
    Ok(())
}

/// Номер проекта для нового сервера: от его идентификатора, чтобы у одного
/// сервера он был одним и тем же при каждой попытке, и мимо занятых.
pub fn choose_id(server: Uuid, taken: &HashSet<u32>) -> u32 {
    let start = (server.as_u128() % u128::from(ID_SPAN)) as u32;
    (0..ID_SPAN)
        .map(|step| FIRST_ID + (start + step) % ID_SPAN)
        .find(|id| !taken.contains(id))
        .unwrap_or(FIRST_ID)
}

/// Project quota на томе каталога данных.
pub struct Projects {
    fs: Arc<dyn ProjectFs>,
    servers_dir: PathBuf,
    /// Один выбор номера за раз: двое разом взяли бы один и тот же.
    choosing: parking_lot::Mutex<()>,
}

impl Projects {
    pub fn new(fs: Arc<dyn ProjectFs>, servers_dir: PathBuf) -> Self {
        Self {
            fs,
            servers_dir,
            choosing: parking_lot::Mutex::new(()),
        }
    }

    /// Назначить серверу проект, если его ещё нет, и поставить лимит.
    /// Блокирующее: первое назначение обходит весь каталог сервера.
    pub fn apply(
        &self,
        server: Uuid,
        root: &Path,
        limits: &mut super::quota::Limits,
    ) -> std::io::Result<()> {
        let id = match limits.project_id {
            Some(id) => id,
            None => {
                let id = {
                    let _one = self.choosing.lock();
                    let mut taken = self.taken();
                    loop {
                        let id = choose_id(server, &taken);
                        if !self.fs.in_use(id)? {
                            break id;
                        }
                        taken.insert(id);
                    }
                };
                self.fs.assign(root, id)?;
                id
            }
        };
        let bytes = (limits.disk_mb.max(0) as u64).saturating_mul(1024 * 1024);
        self.fs.set_limit(id, bytes)?;
        limits.project_id = Some(id);
        Ok(())
    }

    pub fn usage(&self, id: u32) -> std::io::Result<u64> {
        self.fs.usage(id)
    }

    /// Сервер удалён: лимит его номера снимается, номер свободен.
    pub fn release(&self, id: u32) {
        if let Err(e) = self.fs.set_limit(id, 0) {
            tracing::warn!(id, error = %e, "лимит проекта не снят");
        }
    }

    /// Номера, уже записанные за серверами этой ноды.
    fn taken(&self) -> HashSet<u32> {
        let Ok(entries) = std::fs::read_dir(&self.servers_dir) else {
            return HashSet::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| super::quota::read_limits(&e.path()).project_id)
            .collect()
    }
}

/// Решить при старте, чем держать квоту. Отчёт уезжает мастеру в `NodeInfo`.
pub fn detect(data_dir: &Path, wanted: bool) -> (Option<Projects>, DiskQuotaDiag) {
    let walk = |reason: String| {
        tracing::info!(%reason, "квота диска — обходом каталога");
        (
            None,
            DiskQuotaDiag {
                mode: "walk".into(),
                reason: Some(reason),
            },
        )
    };
    if !wanted {
        return walk("project_quota = false in the config".into());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let Some(mount) = mount_of(&mountinfo, data_dir) else {
        return walk("the volume of the data directory is unknown".into());
    };
    if let Err(reason) = supports(&mount) {
        return walk(reason);
    }
    match super::project_sys::open(data_dir) {
        Ok(fs) => {
            tracing::info!(volume = %mount.point.display(), fs = %mount.fstype, "квота диска — project quota");
            let projects = Projects::new(Arc::new(fs), data_dir.join("servers"));
            let diag = DiskQuotaDiag {
                mode: "project".into(),
                reason: None,
            };
            (Some(projects), diag)
        }
        Err(e) => walk(format!("project quota is not usable: {e}")),
    }
}

#[cfg(test)]
#[path = "project_tests.rs"]
pub(crate) mod tests;
