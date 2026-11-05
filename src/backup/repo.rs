// Файл превышает 150 строк: раскладка хранилища кусков и все операции над ним — одно место, где решается, что лежит на диске.
//! Хранилище кусков сервера: `data_dir/backups/<server>/`.
//!
//! ```text
//! chunks/<2 hex>/<64 hex>   кусок под zstd; имя — blake3 несжатых байт
//! snapshots/<uuid>.snap     манифест снимка
//! .incomplete               упаковка начата и не закончена
//! ```
//!
//! Кусок с данным именем всегда несёт одни и те же байты, поэтому его можно
//! только добавить или убрать — переписывать его незачем. Файл появляется под
//! настоящим именем переименованием уже дописанного временного, и недописанный
//! кусок никогда не выглядит готовым.
//!
//! Каталог лежит вне папки сервера и принадлежит демону, но открывается всё
//! равно мимо ссылок, как и прежние архивы.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::manifest::{Chunk, ChunkId, Manifest};
use crate::fs::nofollow;

const CHUNKS: &str = "chunks";
const SNAPSHOTS: &str = "snapshots";
const SNAP_SUFFIX: &str = ".snap";
const INCOMPLETE: &str = ".incomplete";

/// Уровень zstd для кусков. Регионы уже сжаты zlib, и выше третьего уровня
/// выигрыш — проценты при кратно большем времени упаковки.
const LEVEL: i32 = 3;

pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    /// Хранилище без создания: для чтения и проверки, есть ли в нём снимок.
    pub fn at(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Хранилище с созданными каталогами — для записи.
    pub fn open(dir: &Path) -> Result<Self> {
        nofollow::ensure_dir(dir).context("не создать каталог бэкапов сервера")?;
        nofollow::ensure_dir(&dir.join(CHUNKS)).context("не создать каталог кусков")?;
        nofollow::ensure_dir(&dir.join(SNAPSHOTS)).context("не создать каталог снимков")?;
        Ok(Self::at(dir))
    }

    fn chunk_dir(&self, id: &ChunkId) -> PathBuf {
        self.dir.join(CHUNKS).join(&id.hex()[..2])
    }

    pub fn chunk_path(&self, id: &ChunkId) -> PathBuf {
        self.chunk_dir(id).join(id.hex())
    }

    pub fn snapshot_path(&self, backup: Uuid) -> PathBuf {
        self.dir
            .join(SNAPSHOTS)
            .join(format!("{backup}{SNAP_SUFFIX}"))
    }

    pub fn has_snapshot(&self, backup: Uuid) -> bool {
        std::fs::symlink_metadata(self.snapshot_path(backup)).is_ok_and(|m| m.is_file())
    }

    pub fn has_chunk(&self, id: &ChunkId) -> bool {
        std::fs::symlink_metadata(self.chunk_path(id)).is_ok_and(|m| m.is_file())
    }

    /// Положить кусок, если его ещё нет. Возвращает, сколько байт легло на
    /// диск: ноль — такой кусок уже был. `room` — сколько ещё можно занять;
    /// кусок, который не помещается, не пишется вовсе.
    pub fn put_chunk(&self, id: &ChunkId, data: &[u8], room: Option<u64>) -> Result<u64> {
        if self.has_chunk(id) {
            return Ok(0);
        }
        let packed = zstd::bulk::compress(data, LEVEL)?;
        if room.is_some_and(|room| packed.len() as u64 > room) {
            bail!(OutOfRoom);
        }
        let dir = nofollow::ensure_dir(&self.chunk_dir(id))?;
        write_new(&dir, &id.hex(), &packed, false)?;
        Ok(packed.len() as u64)
    }

    /// Прочитать кусок и сверить с его именем: кусок, тихо испорченный на
    /// диске, не должен лечь в восстановленный мир.
    pub fn read_chunk(&self, chunk: &Chunk) -> Result<Vec<u8>> {
        let mut file = nofollow::open_existing(&self.chunk_path(&chunk.id), false)
            .with_context(|| format!("куска {:?} нет в хранилище", chunk.id))?;
        let mut packed = Vec::new();
        file.read_to_end(&mut packed)?;
        let data = zstd::bulk::decompress(&packed, chunk.len as usize)
            .with_context(|| format!("кусок {:?} не распакован", chunk.id))?;
        if data.len() != chunk.len as usize || ChunkId::of(&data) != chunk.id {
            bail!("кусок {:?} испорчен", chunk.id);
        }
        Ok(data)
    }

    /// Записать манифест — это и есть момент, когда снимок появляется.
    ///
    /// До него все куски сбрасываются на диск: манифест, переживший сбой
    /// питания, не должен ссылаться на куски, которые остались в кеше.
    /// Возвращает размер и sha256 файла манифеста.
    pub fn commit(&self, manifest: &Manifest) -> Result<(u64, String)> {
        let bytes = manifest.encode()?;
        sync_fs(&self.dir)?;
        let dir = nofollow::Dir::open(&self.dir.join(SNAPSHOTS))?;
        write_new(
            &dir,
            &format!("{}{SNAP_SUFFIX}", manifest.backup),
            &bytes,
            true,
        )?;
        use sha2::Digest;
        Ok((
            bytes.len() as u64,
            hex::encode(sha2::Sha256::digest(&bytes)),
        ))
    }

    pub fn manifest(&self, backup: Uuid) -> Result<Manifest> {
        let bytes = nofollow::read(&self.snapshot_path(backup))
            .with_context(|| format!("снимка {backup} нет"))?;
        Manifest::decode(&bytes).with_context(|| format!("снимок {backup}"))
    }

    /// Все снимки хранилища.
    pub fn snapshots(&self) -> Result<Vec<Uuid>> {
        let dir = match nofollow::Dir::open(&self.dir.join(SNAPSHOTS)) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in dir.read_dir()? {
            let name = entry?.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|n| n.strip_suffix(SNAP_SUFFIX))
                .and_then(|n| Uuid::parse_str(n).ok())
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Самый свежий снимок: с ним сверяется следующий, чтобы не читать
    /// заново то, что не менялось. Нечитаемый пропускается — без него
    /// следующий снимок просто прочитает всё.
    pub fn latest(&self) -> Option<Manifest> {
        self.snapshots()
            .ok()?
            .into_iter()
            .filter_map(|id| self.manifest(id).ok())
            .max_by_key(|m| m.created)
    }

    /// Убрать манифест. Куски остаются до сборки мусора: решать, чьи они,
    /// может только она, видя все снимки сразу.
    pub fn remove_snapshot(&self, backup: Uuid) -> Result<bool> {
        let path = self.snapshot_path(backup);
        if std::fs::symlink_metadata(&path).is_err() {
            return Ok(false);
        }
        nofollow::remove_file(&path)?;
        let _ = sync_dir(&self.dir.join(SNAPSHOTS));
        Ok(true)
    }

    /// Отметить начало упаковки. Отметка, пережившая демон, значит, что
    /// упаковка оборвалась и оставила ничьи куски.
    pub fn begin(&self, backup: Uuid) -> Result<()> {
        nofollow::write(&self.dir.join(INCOMPLETE), backup.to_string().as_bytes())
    }

    pub fn end(&self) {
        let _ = nofollow::remove_file(&self.dir.join(INCOMPLETE));
    }

    pub fn interrupted(&self) -> bool {
        std::fs::symlink_metadata(self.dir.join(INCOMPLETE)).is_ok()
    }

    /// Каталоги кусков (`chunks/<2 hex>`), для сборки мусора.
    pub(super) fn chunk_dirs(&self) -> Result<Vec<nofollow::Dir>> {
        let root = match nofollow::Dir::open(&self.dir.join(CHUNKS)) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in root.read_dir()? {
            let name = entry?.file_name();
            if let Ok(dir) = nofollow::Dir::open(&self.dir.join(CHUNKS).join(&name)) {
                out.push(dir);
            }
        }
        Ok(out)
    }

    pub(super) fn snapshots_dir(&self) -> Option<nofollow::Dir> {
        nofollow::Dir::open(&self.dir.join(SNAPSHOTS)).ok()
    }
}

/// Кусок не поместился в отведённое бэкапам место.
#[derive(Debug)]
pub struct OutOfRoom;

impl std::fmt::Display for OutOfRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("снимок перерос отведённое место")
    }
}

impl std::error::Error for OutOfRoom {}

/// Записать файл под именем `name` в сверенном каталоге: во временный рядом и
/// переименованием. Существующий файл с тем же именем заменяется — для куска
/// это те же байты, для манифеста такого не бывает.
fn write_new(dir: &nofollow::Dir, name: &str, bytes: &[u8], durable: bool) -> Result<()> {
    let tmp = dir.entry(format!(".{name}.noro-{}", Uuid::new_v4().simple()));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        if durable {
            file.sync_all()?;
        }
        std::fs::rename(&tmp, dir.entry(name))
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if durable {
        sync_dir(&dir.entry(""))?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Сбросить на диск всё, что записано в файловую систему хранилища.
///
/// Один `syncfs` вместо `fsync` на каждый кусок: первый снимок мира — это
/// десятки тысяч кусков, и синхронная запись каждого растягивала бы его на
/// часы.
#[cfg(target_os = "linux")]
fn sync_fs(dir: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(dir)?;
    // SAFETY: дескриптор открыт и жив до конца вызова.
    if unsafe { libc::syncfs(file.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn sync_fs(_dir: &Path) -> Result<()> {
    // SAFETY: без аргументов и без условий.
    unsafe { libc::sync() };
    Ok(())
}
