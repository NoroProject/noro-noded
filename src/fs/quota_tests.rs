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
            write_limits(&root, &Limits { disk_mb }).unwrap();
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
