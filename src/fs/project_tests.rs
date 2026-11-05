use super::*;
use crate::fs::quota::{read_limits, write_limits, Limits, Quota};
use std::collections::HashMap;

/// Ядро в памяти: какие каталоги помечены, какие лимиты стоят, сколько занято.
#[derive(Default)]
pub struct FakeKernel {
    pub assigned: parking_lot::Mutex<Vec<(PathBuf, u32)>>,
    pub limits: parking_lot::Mutex<HashMap<u32, u64>>,
    pub used: parking_lot::Mutex<HashMap<u32, u64>>,
}

impl ProjectFs for FakeKernel {
    fn assign(&self, root: &Path, id: u32) -> std::io::Result<()> {
        self.assigned.lock().push((root.to_path_buf(), id));
        Ok(())
    }
    fn set_limit(&self, id: u32, bytes: u64) -> std::io::Result<()> {
        self.limits.lock().insert(id, bytes);
        Ok(())
    }
    fn usage(&self, id: u32) -> std::io::Result<u64> {
        Ok(self.used.lock().get(&id).copied().unwrap_or(0))
    }
    fn in_use(&self, id: u32) -> std::io::Result<bool> {
        Ok(self.used.lock().contains_key(&id))
    }
}

const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw,discard,errors=remount-ro
30 22 0:26 / /proc rw,nosuid shared:5 - proc proc rw
41 22 253:0 / /var/lib/noro-noded rw,noatime shared:30 - xfs /dev/mapper/data rw,attr2,inode64,prjquota
42 22 8:17 / /mnt/my\\040disk rw,relatime shared:31 - ext4 /dev/sdb1 rw,prjquota
";

#[test]
fn the_deepest_mount_on_the_path_wins() {
    let m = mount_of(MOUNTINFO, Path::new("/var/lib/noro-noded/servers")).unwrap();
    assert_eq!(m.point, PathBuf::from("/var/lib/noro-noded"));
    assert_eq!(m.fstype, "xfs");
    assert!(m.options.contains(&"prjquota".to_string()));

    let root = mount_of(MOUNTINFO, Path::new("/var/lib/noro")).unwrap();
    assert_eq!(
        root.point,
        PathBuf::from("/"),
        "соседний префикс — не тот том"
    );
}

#[test]
fn escaped_spaces_in_mount_points_are_decoded() {
    let m = mount_of(MOUNTINFO, Path::new("/mnt/my disk/noro")).unwrap();
    assert_eq!(m.point, PathBuf::from("/mnt/my disk"));
    assert_eq!(m.fstype, "ext4");
}

/// Прод: ext4 корневым томом без prjquota — остаётся обход каталога.
#[test]
fn support_needs_xfs_or_ext4_mounted_with_prjquota() {
    let root = mount_of(MOUNTINFO, Path::new("/var/lib/other")).unwrap();
    assert!(supports(&root).unwrap_err().contains("without prjquota"));
    let data = mount_of(MOUNTINFO, Path::new("/var/lib/noro-noded")).unwrap();
    assert!(supports(&data).is_ok());

    let btrfs = Mount {
        point: "/".into(),
        fstype: "btrfs".into(),
        options: vec!["prjquota".into()],
    };
    assert!(supports(&btrfs).unwrap_err().contains("no project quota"));
}

#[test]
fn a_project_id_is_stable_and_steps_over_taken_ones() {
    let server = Uuid::from_u128(0xdead_beef);
    let first = choose_id(server, &HashSet::new());
    assert_eq!(first, choose_id(server, &HashSet::new()));
    assert!(first >= FIRST_ID);
    let next = choose_id(server, &HashSet::from([first]));
    assert_eq!(next, first + 1);
}

#[test]
fn ids_wrap_inside_their_span() {
    let server = Uuid::from_u128(u128::from(ID_SPAN - 1));
    let last = choose_id(server, &HashSet::new());
    assert_eq!(last, FIRST_ID + ID_SPAN - 1);
    assert_eq!(choose_id(server, &HashSet::from([last])), FIRST_ID);
}

struct Node {
    data: PathBuf,
    kernel: Arc<FakeKernel>,
}

impl Node {
    fn new() -> Self {
        let data = std::env::temp_dir().join(format!("noded-project-{}", Uuid::new_v4()));
        std::fs::create_dir_all(data.join("servers")).unwrap();
        Self {
            data,
            kernel: Arc::new(FakeKernel::default()),
        }
    }

    fn server(&self, id: Uuid) -> PathBuf {
        let root = self.data.join("servers").join(id.to_string());
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn quota(&self) -> Quota {
        let projects = Projects::new(self.kernel.clone(), self.data.join("servers"));
        Quota::default().with_projects(
            Some(projects),
            DiskQuotaDiag {
                mode: "project".into(),
                reason: None,
            },
        )
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// Смена лимита в админке доходит до ядра сразу — без пересборки.
#[tokio::test]
async fn a_new_limit_reaches_the_kernel_at_once_and_marks_the_tree_once() {
    let node = Node::new();
    let server = Uuid::new_v4();
    let root = node.server(server);
    let quota = node.quota();

    quota.set_limit(server, &root, 1024).await.unwrap();
    let id = read_limits(&root).project_id.expect("номер записан");
    assert_eq!(node.kernel.limits.lock()[&id], 1024 * 1024 * 1024);

    quota.set_limit(server, &root, 2048).await.unwrap();
    assert_eq!(node.kernel.limits.lock()[&id], 2048 * 1024 * 1024);
    assert_eq!(read_limits(&root).disk_mb, 2048);
    assert_eq!(
        node.kernel.assigned.lock().len(),
        1,
        "каталог помечается один раз, дальше меняется только лимит"
    );
}

/// Занятое берётся у ядра, не обходом: файл на диске ему не нужен.
#[tokio::test]
async fn usage_comes_from_the_kernel_and_is_never_stale() {
    let node = Node::new();
    let server = Uuid::new_v4();
    let root = node.server(server);
    let quota = node.quota();
    quota.set_limit(server, &root, 100).await.unwrap();
    let id = read_limits(&root).project_id.unwrap();

    node.kernel.used.lock().insert(id, 90 * 1024 * 1024);
    let usage = quota.usage(server, &root).await;
    assert_eq!(usage.used_mb(), 90);
    assert_eq!(usage.limit_bytes, 100 * 1024 * 1024);

    node.kernel.used.lock().insert(id, 10 * 1024 * 1024);
    assert_eq!(quota.usage(server, &root).await.used_mb(), 10);
}

/// Номер, под которым у ядра уже что-то лежит, чужой — его не берут.
#[tokio::test]
async fn a_busy_project_id_is_skipped() {
    let node = Node::new();
    let server = Uuid::new_v4();
    let root = node.server(server);
    let natural = choose_id(server, &HashSet::new());
    node.kernel.used.lock().insert(natural, 1);

    node.quota().set_limit(server, &root, 10).await.unwrap();
    assert_eq!(read_limits(&root).project_id, Some(natural + 1));
}

#[tokio::test]
async fn ids_already_written_for_other_servers_are_not_reused() {
    let node = Node::new();
    let (a, b) = (Uuid::from_u128(5), Uuid::from_u128(5 + u128::from(ID_SPAN)));
    assert_eq!(choose_id(a, &HashSet::new()), choose_id(b, &HashSet::new()));
    let quota = node.quota();
    quota.set_limit(a, &node.server(a), 10).await.unwrap();
    quota.set_limit(b, &node.server(b), 10).await.unwrap();
    let ida = read_limits(&node.server(a)).project_id.unwrap();
    let idb = read_limits(&node.server(b)).project_id.unwrap();
    assert_ne!(ida, idb);
}

#[tokio::test]
async fn an_old_server_is_adopted_and_a_deleted_one_released() {
    let node = Node::new();
    let server = Uuid::new_v4();
    let root = node.server(server);
    write_limits(
        &root,
        &Limits {
            disk_mb: 500,
            project_id: None,
        },
    )
    .unwrap();
    let quota = node.quota();

    quota.adopt(server, &root);
    let id = read_limits(&root).project_id.expect("переведён");
    assert_eq!(node.kernel.limits.lock()[&id], 500 * 1024 * 1024);

    quota.release(&root);
    assert_eq!(node.kernel.limits.lock()[&id], 0);
}

/// Без лимитов в файле сервер не трогается: его потолок неизвестен.
#[test]
fn a_server_without_limits_is_not_adopted() {
    let node = Node::new();
    let server = Uuid::new_v4();
    let root = node.server(server);
    node.quota().adopt(server, &root);
    assert!(node.kernel.assigned.lock().is_empty());
}
