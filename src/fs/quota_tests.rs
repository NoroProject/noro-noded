//! Проверки квоты: считаем ли мы то, что надо, и отказываем ли когда надо.

use super::*;

struct Sandbox {
    root: std::path::PathBuf,
}

impl Sandbox {
    fn new(disk_mb: i64) -> Self {
        let root = std::env::temp_dir().join(format!("noded-quota-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("world")).unwrap();
        if disk_mb > 0 {
            write_limits(
                &root,
                &Limits {
                    disk_mb,
                    ..Limits::default()
                },
            )
            .unwrap();
        }
        Self { root }
    }

    fn file(&self, name: &str, bytes: usize) {
        std::fs::write(self.root.join(name), vec![0u8; bytes]).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_file(limits_path(&self.root));
    }
}

#[tokio::test]
async fn usage_counts_files_and_reads_the_limit() {
    let s = Sandbox::new(10);
    s.file("world/region.mca", 2 * 1024 * 1024);

    let usage = Quota::default().usage(Uuid::new_v4(), &s.root).await;

    assert_eq!(usage.used_mb(), 2);
    assert_eq!(usage.limit_bytes, 10 * 1024 * 1024);
}

/// Главная проверка файла: запись, которая не влезает, отвергается до того, как
/// начнётся — а не после того, как диск ноды кончится.
#[tokio::test]
async fn a_write_over_the_limit_is_refused() {
    let s = Sandbox::new(4);
    let server = Uuid::new_v4();
    s.file("big.jar", 3 * 1024 * 1024);

    let quota = Quota::default();
    assert!(
        quota.check(server, &s.root, 512 * 1024).await.is_ok(),
        "влезает"
    );
    assert!(
        quota.check(server, &s.root, 2 * 1024 * 1024).await.is_err(),
        "не влезает — отказ"
    );
}

/// Сервер, заведённый до появления квоты, лимита не имеет. Отказывать ему
/// задним числом нельзя: он не сделал ничего нового.
#[tokio::test]
async fn without_a_limit_everything_passes() {
    let s = Sandbox::new(0);
    let quota = Quota::default();

    assert!(quota
        .check(Uuid::new_v4(), &s.root, 100 * 1024 * 1024)
        .await
        .is_ok());
}

/// Между обходами каталога записи учитываются на память: пересчитывать тысячи
/// файлов на каждый залитый мод — это секунды на ровном месте.
#[tokio::test]
async fn writes_between_measurements_are_accounted() {
    let s = Sandbox::new(4);
    let server = Uuid::new_v4();
    s.file("a.jar", 1024 * 1024);

    let quota = Quota::default();
    quota.usage(server, &s.root).await;
    quota.add(server, 3 * 1024 * 1024);

    assert!(
        quota.check(server, &s.root, 1024).await.is_err(),
        "добавленное учтено без нового обхода"
    );
}

#[tokio::test]
async fn the_service_directory_counts_too() {
    let s = Sandbox::new(10);
    std::fs::create_dir_all(s.root.join(".noro")).unwrap();
    std::fs::write(s.root.join(".noro/agent.jar"), vec![0u8; 1024 * 1024]).unwrap();

    // Агент и authlib занимают место на том же диске: прятать их от замера
    // значит обещать владельцу больше, чем у него есть.
    assert!(
        Quota::default()
            .usage(Uuid::new_v4(), &s.root)
            .await
            .used_mb()
            >= 1
    );
}

#[test]
fn a_huge_request_does_not_wrap_around() {
    let usage = Usage {
        used_bytes: 1024,
        limit_bytes: 10 * 1024 * 1024,
    };
    assert!(usage.fits(u64::MAX).is_err());
    assert!(usage.fits(u64::MAX - 512).is_err());
}

/// Лимит живёт вне каталога, который видит контейнер: переписать его оттуда
/// нечем.
#[tokio::test]
async fn the_limit_is_kept_outside_the_server_directory() {
    let s = Sandbox::new(4);
    assert!(!limits_path(&s.root).starts_with(&s.root));
    std::fs::create_dir_all(s.root.join(".noro")).unwrap();
    std::fs::write(s.root.join(".noro/limits.json"), r#"{"disk_mb":0}"#).unwrap();

    let usage = Quota::default().usage(Uuid::new_v4(), &s.root).await;
    assert_eq!(
        usage.limit_bytes,
        4 * 1024 * 1024,
        "подделка внутри не действует"
    );
}

#[tokio::test]
async fn an_old_limit_is_carried_over_once() {
    let s = Sandbox::new(0);
    std::fs::create_dir_all(s.root.join(".noro")).unwrap();
    std::fs::write(s.root.join(".noro/limits.json"), r#"{"disk_mb":7}"#).unwrap();

    assert_eq!(read_limits(&s.root).disk_mb, 7);
    assert!(limits_path(&s.root).exists(), "перенесён");
    assert!(!s.root.join(".noro/limits.json").exists());
}

/// Смена лимита в админке доходит до квоты сразу, без пересборки контейнера:
/// прежде новый потолок ждал следующего `ServerUpdate`.
#[tokio::test]
async fn a_new_limit_applies_at_once() {
    let s = Sandbox::new(10);
    s.file("world/region.mca", 5 * 1024 * 1024);
    let quota = Quota::default();
    let server = Uuid::new_v4();
    assert!(quota.check(server, &s.root, 1024 * 1024).await.is_ok());

    let usage = quota.set_limit(server, &s.root, 5).await.unwrap();
    assert_eq!(usage.limit_bytes, 5 * 1024 * 1024);
    assert!(quota.check(server, &s.root, 1024 * 1024).await.is_err());
    assert_eq!(read_limits(&s.root).disk_mb, 5);
}
