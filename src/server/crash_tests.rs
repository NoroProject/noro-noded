use super::*;
use std::path::PathBuf;
use std::time::Duration;

struct Scratch {
    base: PathBuf,
    root: PathBuf,
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("noded-crash-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        // Сверенный каталог сравнивает путь буквально, а временный каталог на
        // macOS лежит за ссылкой.
        let base = base.canonicalize().unwrap();
        let root = base.join("servers/s");
        std::fs::create_dir_all(root.join("crash-reports")).unwrap();
        let dir = base.join("crashes/s");
        Self { base, root, dir }
    }

    fn crash(&self, since: SystemTime, console: &[&str]) {
        record_in(
            &self.dir,
            &self.root,
            Crash {
                state: PowerState::Crashed,
                exit_code: Some(1),
                run_started: since,
                console: console.iter().map(|s| s.to_string()).collect(),
            },
        )
        .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn the_tail_keeps_the_last_lines_only() {
    let mut tail = Tail::default();
    let lines: Vec<String> = (0..TAIL_LINES + 50).map(|i| i.to_string()).collect();
    tail.push(&lines[..10]);
    tail.push(&lines[10..]);
    let kept = tail.take();
    assert_eq!(kept.len(), TAIL_LINES);
    assert_eq!(kept.first().unwrap(), "50");
    assert_eq!(kept.last().unwrap(), &(TAIL_LINES + 49).to_string());
    assert!(tail.take().is_empty(), "взятый хвост начинается заново");
}

#[test]
fn a_long_line_is_cut_on_a_character() {
    let mut tail = Tail::default();
    tail.push(&["я".repeat(MAX_LINE)]);
    let kept = tail.take();
    assert!(kept[0].len() <= MAX_LINE);
}

#[test]
fn the_crash_report_of_this_run_is_kept() {
    let s = Scratch::new();
    let before = SystemTime::now() - Duration::from_secs(5);
    std::fs::write(
        s.root.join("crash-reports/crash-1.txt"),
        "Description: boom",
    )
    .unwrap();

    s.crash(before, &["[Server thread] ERROR", "at foo.Bar"]);

    let reports = read_in(&s.dir, None).unwrap();
    let report = reports.report.unwrap();
    assert_eq!(report.console, vec!["[Server thread] ERROR", "at foo.Bar"]);
    let file = report.crash_file.unwrap();
    assert_eq!(file.name, "crash-1.txt");
    assert_eq!(file.content, "Description: boom");
}

/// Отчёт прошлого падения к этому отношения не имеет.
#[test]
fn an_older_crash_report_is_not_attributed() {
    let s = Scratch::new();
    std::fs::write(s.root.join("crash-reports/old.txt"), "old").unwrap();
    s.crash(SystemTime::now() + Duration::from_secs(60), &[]);
    assert!(read_in(&s.dir, None)
        .unwrap()
        .report
        .unwrap()
        .crash_file
        .is_none());
}

/// Ссылка в `crash-reports` от процесса сервера — путь к файлу хоста.
#[test]
fn a_linked_crash_report_is_not_read() {
    let s = Scratch::new();
    let host = s.base.join("noded.toml");
    std::fs::write(&host, "token = secret").unwrap();
    std::os::unix::fs::symlink(&host, s.root.join("crash-reports/crash.txt")).unwrap();

    s.crash(SystemTime::UNIX_EPOCH, &[]);

    let report = read_in(&s.dir, None).unwrap().report.unwrap();
    assert!(report.crash_file.is_none());
}

#[test]
fn only_the_newest_crashes_are_kept_and_any_can_be_asked_for() {
    let s = Scratch::new();
    for i in 0..KEEP + 2 {
        s.crash(SystemTime::now(), &[&i.to_string()]);
        // Имя — миллисекунды: два падения в одну миллисекунду слились бы.
        std::thread::sleep(Duration::from_millis(3));
    }

    let all = read_in(&s.dir, None).unwrap();
    assert_eq!(all.crashes.len(), KEEP);
    assert_eq!(all.report.unwrap().console, vec![(KEEP + 1).to_string()]);

    let oldest = all.crashes.last().unwrap().id.clone();
    let picked = read_in(&s.dir, Some(&oldest)).unwrap().report.unwrap();
    assert_eq!(picked.console, vec!["2".to_string()]);
}
