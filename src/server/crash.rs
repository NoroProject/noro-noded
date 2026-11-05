// Файл превышает 150 строк: хвост консоли, поиск свежего crash-report и хранение отпечатков — одна запись о падении.
//! Отпечаток падения: что сервер успел сказать перед смертью.
//!
//! После падения владелец открывает консоль, а там уже лог следующего запуска:
//! автоподъём стартует сервер через секунды, и стек-трейс уезжает за экран.
//! Поэтому демон сам сохраняет последние строки консоли и свежий
//! `crash-reports/*.txt` того запуска — вне каталога сервера, куда процесс
//! сервера не дотянется, — и отдаёт их мастеру по `CrashReport`.

use anyhow::{Context, Result};
use schema::noded::{CrashFile, CrashReport, CrashReports, CrashSummary, PowerState};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::Config;
use crate::fs::nofollow;

/// Сколько строк консоли хранится.
pub const TAIL_LINES: usize = 200;
/// Сколько падений помнится на сервер: старше — уже не про нынешнюю беду.
const KEEP: usize = 5;
/// Строка консоли в отпечатке. Сама консоль пропускает до 16 КБ на строку, а
/// двести таких — это мегабайты в одном ответе мастеру.
const MAX_LINE: usize = 2 * 1024;
/// Сколько читать из crash-report: суть — в начале, дальше списки модов.
const MAX_CRASH_FILE: u64 = 256 * 1024;

/// Последние строки консоли текущего запуска.
#[derive(Default)]
pub struct Tail {
    lines: VecDeque<String>,
}

impl Tail {
    pub fn push(&mut self, lines: &[String]) {
        let skip = lines.len().saturating_sub(TAIL_LINES);
        for line in &lines[skip..] {
            if self.lines.len() == TAIL_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(cut(line));
        }
    }

    pub fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines).into()
    }
}

fn cut(line: &str) -> String {
    if line.len() <= MAX_LINE {
        return line.to_string();
    }
    let mut end = MAX_LINE;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_string()
}

/// Хранилище отпечатков. Без конфига (в тестах реестра) ничего не пишет.
#[derive(Clone, Default)]
pub struct CrashStore {
    cfg: Option<Arc<Config>>,
}

/// Что известно о падении в момент, когда консоль закрылась.
pub struct Crash {
    pub state: PowerState,
    pub exit_code: Option<i32>,
    /// Когда начался упавший запуск: crash-report старше него — от прошлой беды.
    pub run_started: SystemTime,
    pub console: Vec<String>,
}

impl CrashStore {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg: Some(cfg) }
    }

    /// Записать отпечаток и забыть самые старые.
    pub fn record(&self, server: Uuid, crash: Crash) -> Result<()> {
        let Some(cfg) = &self.cfg else {
            return Ok(());
        };
        record_in(&cfg.crash_dir(server), &cfg.server_dir(server), crash)
    }

    /// Список падений и одно из них целиком — названное или последнее.
    pub fn read(&self, server: Uuid, crash: Option<&str>) -> Result<CrashReports> {
        let Some(cfg) = &self.cfg else {
            return Ok(CrashReports::default());
        };
        read_in(&cfg.crash_dir(server), crash)
    }
}

pub(crate) fn record_in(dir: &Path, root: &Path, crash: Crash) -> Result<()> {
    let at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let id = at.as_millis().to_string();
    let report = CrashReport {
        summary: CrashSummary {
            id: id.clone(),
            at: at.as_secs() as i64,
            state: crash.state,
            exit_code: crash.exit_code,
        },
        console: crash.console,
        crash_file: fresh_crash_file(root, crash.run_started),
    };

    nofollow::ensure_dir(dir.parent().context("у каталога падений нет родителя")?)?;
    nofollow::ensure_dir(dir)?;
    nofollow::write(
        &dir.join(format!("{id}.json")),
        &serde_json::to_vec(&report)?,
    )?;

    for old in ids(dir).into_iter().skip(KEEP) {
        let _ = nofollow::remove_file(&dir.join(format!("{old}.json")));
    }
    Ok(())
}

pub(crate) fn read_in(dir: &Path, crash: Option<&str>) -> Result<CrashReports> {
    let ids = ids(dir);
    let mut crashes = Vec::new();
    let mut report = None;
    for id in &ids {
        let Ok(bytes) = nofollow::read(&dir.join(format!("{id}.json"))) else {
            continue;
        };
        let Ok(full) = serde_json::from_slice::<CrashReport>(&bytes) else {
            continue;
        };
        crashes.push(full.summary.clone());
        let wanted = crash.map_or(report.is_none(), |c| c == id.to_string());
        if wanted {
            report = Some(full);
        }
    }
    Ok(CrashReports { crashes, report })
}

/// Отпечатки на диске, новые первыми. Имя — миллисекунды, поэтому порядок
/// чисел — порядок времени.
fn ids(dir: &Path) -> Vec<u128> {
    let Ok(dir) = nofollow::Dir::open(dir) else {
        return Vec::new();
    };
    let Ok(entries) = dir.read_dir() else {
        return Vec::new();
    };
    let mut ids: Vec<u128> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json")?.parse().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Самый свежий `crash-reports/*.txt`, записанный за упавший запуск.
///
/// Каталог принадлежит процессу сервера: открывается сверенным, файл —
/// без ссылок, и читается не больше предела.
fn fresh_crash_file(root: &Path, since: SystemTime) -> Option<CrashFile> {
    let path = root.join("crash-reports");
    let dir = nofollow::Dir::open(&path).ok()?;
    let newest = dir
        .read_dir()
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".txt"))
        .filter_map(|e| {
            let meta = std::fs::symlink_metadata(dir.entry(e.file_name())).ok()?;
            let modified = meta.modified().ok()?;
            (meta.file_type().is_file() && modified >= since).then_some((modified, e.file_name()))
        })
        .max_by_key(|(modified, _)| *modified)?;

    let name = newest.1.to_string_lossy().into_owned();
    let file = nofollow::open_existing(&path.join(&newest.1), false).ok()?;
    let size = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    file.take(MAX_CRASH_FILE).read_to_end(&mut bytes).ok()?;
    Some(CrashFile {
        name,
        content: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: size > MAX_CRASH_FILE,
    })
}

#[cfg(all(test, unix))]
#[path = "crash_tests.rs"]
mod tests;
