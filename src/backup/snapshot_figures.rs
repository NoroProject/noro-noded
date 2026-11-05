//! Цифры для отчёта о снимках на синтетическом мире:
//! `cargo test --release synthetic_world -- --ignored --nocapture`.

use super::tests::{tree, Scratch};
use crate::backup::store;
use rand::{Rng, SeedableRng};

/// Регион, похожий на настоящий: таблица смещений и сжатые чанки, каждый
/// дополнен нулями до сектора в 4 КБ.
fn region(seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut out = vec![0u8; 8192];
    rng.fill(&mut out[..8192]);
    for _ in 0..1024 {
        let len = rng.gen_range(2048..10240);
        let start = out.len();
        out.resize(start + len, 0);
        rng.fill(&mut out[start..start + len]);
        out.resize(out.len().next_multiple_of(4096), 0);
    }
    out
}

/// Переписать `count` чанков региона на месте и обновить таблицу времён — так
/// выглядит регион, где день побегали игроки.
fn touch_region(data: &mut [u8], seed: u64, count: usize) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    rng.fill(&mut data[4096..8192]);
    for _ in 0..count {
        let at = rng.gen_range(2..data.len() / 4096 - 2) * 4096;
        rng.fill(&mut data[at..at + 3000]);
    }
}

#[test]
#[ignore]
fn synthetic_world_figures() {
    let s = Scratch::new();
    let regions = 128;
    for i in 0..regions {
        std::fs::write(s.root.join(format!("world/region/r.{i}.0.mca")), region(i)).unwrap();
    }
    let world: u64 = tree(&s.root).iter().map(|(_, b)| b.len() as u64).sum();
    let mb = |b: u64| b as f64 / 1024.0 / 1024.0;

    let t = std::time::Instant::now();
    let tgz = s.base.join("full.tar.gz");
    let full = crate::backup::create::pack_for_test(&s.root, &tgz);
    eprintln!(
        "мир {:.1} МБ; полный tar.gz {:.1} МБ за {:?}",
        mb(world),
        mb(full),
        t.elapsed()
    );

    let t = std::time::Instant::now();
    let (_, first) = s.take(None).unwrap();
    eprintln!(
        "первый снимок +{:.1} МБ за {:?}",
        mb(first.bytes),
        t.elapsed()
    );

    for i in 0..6 {
        let path = s.root.join(format!("world/region/r.{i}.0.mca"));
        let mut data = std::fs::read(&path).unwrap();
        touch_region(&mut data, 1000 + i, 40);
        std::fs::write(&path, data).unwrap();
    }
    let t = std::time::Instant::now();
    let (second_id, second) = s.take(None).unwrap();
    eprintln!(
        "второй снимок (6 регионов из {regions} по 40 чанков) +{:.1} МБ за {:?}; хранилище {:.1} МБ",
        mb(second.bytes),
        t.elapsed(),
        mb(store::usage(&s.dir))
    );

    let t = std::time::Instant::now();
    let out = s.restored(second_id, &[]);
    eprintln!("восстановление за {:?}", t.elapsed());
    assert_eq!(tree(&out), tree(&s.root));

    let t = std::time::Instant::now();
    let mut sink = Counter(0);
    crate::backup::stream::write_tar_gz(&s.repo(), second_id, &mut sink).unwrap();
    eprintln!("tar.gz на лету {:.1} МБ за {:?}", mb(sink.0), t.elapsed());
}

struct Counter(u64);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
