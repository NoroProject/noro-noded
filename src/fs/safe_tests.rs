//! Матрица побегов. Каждая строка здесь — способ, которым чужой каталог
//! становится доступен, если проверка чуть слабее, чем нужно.

use super::*;
use std::fs;

struct Sandbox {
    root: PathBuf,
    outside: PathBuf,
    _dir: tempdir::TempDir,
}

/// Минимальный временный каталог: тянуть `tempfile` в зависимости ради тестов
/// не хочется, а удалить за собой надо обязательно.
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> std::io::Result<Self> {
            let base = std::env::temp_dir().join(format!(
                "noded-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&base)?;
            Ok(Self(base))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempdir::TempDir::new("safe").expect("временный каталог");
        let root = dir.path().join("server");
        let outside = dir.path().join("secrets");
        fs::create_dir_all(root.join("mods")).expect("mods");
        fs::create_dir_all(&outside).expect("secrets");
        fs::write(outside.join("token.txt"), "s3cret").expect("файл снаружи");
        fs::write(root.join("server.properties"), "level-name=world").expect("файл внутри");
        Self {
            root,
            outside,
            _dir: dir,
        }
    }
}

#[test]
fn a_plain_path_resolves_inside_the_root() {
    let s = Sandbox::new();
    let path = resolve(&s.root, "mods/jei.jar").expect("обычный путь");
    assert!(path.starts_with(s.root.canonicalize().unwrap()));
    assert!(path.ends_with("mods/jei.jar"));
}

/// Разрешённым путём должно быть можно **пользоваться**.
///
/// Сравнение через `ends_with` этого не ловит: оно идёт по компонентам и не
/// видит завершающего разделителя, а система на `file.jar/` отвечает ENOTDIR.
#[test]
fn a_resolved_file_can_actually_be_opened() {
    let s = Sandbox::new();
    let path = resolve(&s.root, "server.properties").expect("обычный путь");

    std::fs::File::open(&path).expect("файл открывается");
    assert!(
        !path.to_string_lossy().ends_with('/'),
        "путь оканчивается разделителем: {}",
        path.display()
    );
}

#[test]
fn dot_dot_is_refused() {
    let s = Sandbox::new();
    for attempt in [
        "../secrets/token.txt",
        "mods/../../secrets/token.txt",
        "..",
        "mods/..",
    ] {
        assert_eq!(
            resolve(&s.root, attempt),
            Err(PathError::Escapes),
            "должно быть отвергнуто: {attempt}"
        );
    }
}

#[test]
fn an_absolute_path_is_refused() {
    let s = Sandbox::new();
    assert_eq!(resolve(&s.root, "/etc/passwd"), Err(PathError::Invalid));
}

#[test]
fn a_nul_byte_is_refused() {
    let s = Sandbox::new();
    assert_eq!(resolve(&s.root, "mods/\0evil"), Err(PathError::Invalid));
}

/// Главный случай: ссылку кладут файловым менеджером, а ходят по ней потом —
/// например, через SFTP. Проверка строки такое не ловит в принципе.
#[test]
#[cfg(unix)]
fn a_symlink_pointing_outside_leads_nowhere() {
    let s = Sandbox::new();
    std::os::unix::fs::symlink(&s.outside, s.root.join("escape")).expect("симлинк");

    assert_eq!(
        resolve(&s.root, "escape/token.txt"),
        Err(PathError::Escapes),
        "переход по ссылке наружу обязан быть отвергнут"
    );
    assert_eq!(resolve(&s.root, "escape"), Err(PathError::Escapes));
}

/// Ссылка внутрь корня безобидна как путь, но открывать надо файл, а не ссылку.
#[test]
#[cfg(unix)]
fn a_symlink_inside_the_root_is_still_not_opened() {
    let s = Sandbox::new();
    std::os::unix::fs::symlink(s.root.join("server.properties"), s.root.join("props.link"))
        .expect("симлинк");

    assert!(resolve(&s.root, "props.link").is_ok(), "как путь — внутри");
    assert_eq!(
        resolve_existing(&s.root, "props.link"),
        Err(PathError::Escapes),
        "а открывать ссылку незачем"
    );
}

/// Файл ещё не создан — путь обязан разрешиться, иначе ничего не записать.
#[test]
fn a_path_that_does_not_exist_yet_still_resolves() {
    let s = Sandbox::new();
    let path = resolve(&s.root, "config/new/deep.toml").expect("несуществующий путь");
    assert!(path.starts_with(s.root.canonicalize().unwrap()));
    assert!(!path.exists());
}

/// Но несуществующий хвост за пределами корня — по-прежнему побег.
#[test]
#[cfg(unix)]
fn a_new_file_behind_a_symlink_is_refused() {
    let s = Sandbox::new();
    std::os::unix::fs::symlink(&s.outside, s.root.join("escape")).expect("симлинк");
    assert_eq!(
        resolve(&s.root, "escape/planted.txt"),
        Err(PathError::Escapes)
    );
}

#[test]
fn current_dir_components_are_harmless() {
    let s = Sandbox::new();
    let path = resolve(&s.root, "./mods/./jei.jar").expect("./ допустим");
    assert!(path.ends_with("mods/jei.jar"));
}

/// Служебный каталог прячется по имени, и ссылка — очевидный способ это имя
/// не называть. Проверяется, куда путь привёл, а не как он выглядел.
#[test]
#[cfg(unix)]
fn a_symlink_cannot_smuggle_the_service_directory_out() {
    let s = Sandbox::new();
    fs::create_dir_all(s.root.join(".noro")).expect(".noro");
    fs::write(s.root.join(".noro/agent-secret"), "noroagent_x").expect("секрет");
    std::os::unix::fs::symlink(s.root.join(".noro"), s.root.join("mods/service")).expect("симлинк");

    assert_eq!(
        resolve(&s.root, "mods/service/agent-secret"),
        Err(PathError::Escapes),
        "через ссылку служебный каталог доставать нельзя"
    );
    assert_eq!(
        resolve(&s.root, ".noro/agent-secret"),
        Err(PathError::Escapes)
    );
}

#[test]
fn the_service_directory_is_hidden() {
    assert!(is_hidden(".noro"));
    assert!(is_hidden(".noro/authlib-injector.jar"));
    assert!(!is_hidden("mods/.noro-ish.jar"));
    assert!(!is_hidden("config/noro.toml"));
}
