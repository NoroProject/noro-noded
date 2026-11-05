use super::*;
use crate::backup::snapshot::take;

/// Снимок, отданный наружу, — обычный tar.gz с теми же файлами, правами и
/// пустыми каталогами.
#[test]
fn a_snapshot_streams_as_a_plain_tar_gz() {
    let base = std::env::temp_dir().join(format!("noded-stream-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();
    let root = base.join("server");
    std::fs::create_dir_all(root.join("world/region")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    let mut region = vec![0u8; 3 << 20];
    rand::Rng::fill(&mut rand::thread_rng(), &mut region[..]);
    std::fs::write(root.join("world/region/r.0.0.mca"), &region).unwrap();
    std::fs::write(root.join("server.properties"), b"motd=x").unwrap();

    let repo = Repo::open(&base.join("backups")).unwrap();
    let id = Uuid::new_v4();
    take(&root, &repo, id, &[], None).unwrap();

    let mut bytes = Vec::new();
    write_tar_gz(&repo, id, &mut bytes).unwrap();

    let mut seen = Vec::new();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry
            .path()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).unwrap();
        if path == "world/region/r.0.0.mca" {
            assert_eq!(body, region);
        }
        seen.push((path, entry.header().entry_type().is_dir()));
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("empty".to_string(), true),
            ("server.properties".to_string(), false),
            ("world".to_string(), true),
            ("world/region".to_string(), true),
            ("world/region/r.0.0.mca".to_string(), false),
        ]
    );
    std::fs::remove_dir_all(base).unwrap();
}

/// Ушедший получатель обрывает сборку, а не держит поток до конца мира.
#[tokio::test]
async fn a_dropped_receiver_stops_the_stream() {
    let base = std::env::temp_dir().join(format!("noded-stream-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();
    let root = base.join("server");
    std::fs::create_dir_all(&root).unwrap();
    let mut data = vec![0u8; 4 << 20];
    rand::Rng::fill(&mut rand::thread_rng(), &mut data[..]);
    std::fs::write(root.join("big.dat"), &data).unwrap();
    let repo = Repo::open(&base.join("backups")).unwrap();
    let id = Uuid::new_v4();
    take(&root, &repo, id, &[], None).unwrap();

    use futures_util::StreamExt;
    let mut stream = Box::pin(tar_gz_stream(base.join("backups"), id));
    let first = stream.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(stream);
    std::fs::remove_dir_all(base).unwrap();
}
