//! Манифест снимка: какие файлы и каталоги лежали в папке сервера и из каких
//! кусков собран каждый файл.
//!
//! Манифест — единственное, что делает набор кусков бэкапом. Он пишется
//! последним, когда все его куски уже на диске, и пока его нет, снимка нет:
//! оборванная упаковка оставляет только ничьи куски, их убирает сборка мусора.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Версия формата. Неизвестную версию демон не читает: восстановить мир по
/// манифесту, который понят наполовину, хуже, чем отказать.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub backup: Uuid,
    /// Unix-секунды. По ним выбирается снимок, с которым сверяется следующий.
    pub created: i64,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Относительно корня сервера, через `/`.
    pub path: String,
    /// Каталог. Всё прочее — обычный файл: ссылок формат не выражает вовсе.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dir: bool,
    pub mode: u32,
    pub mtime: i64,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<Chunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<Stamp>,
}

/// Отпечаток файла, по которому следующий снимок верит, что файл не менялся,
/// и не читает его заново.
///
/// `ctime` здесь главный: время изменения владелец выставит любое, а `ctime`
/// ядро двигает само на каждой записи и смене метаданных.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub len: u32,
}

/// blake3 несжатых байт куска — и его имя в хранилище.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(pub [u8; 32]);

impl ChunkId {
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).ok()?;
        Some(Self(out))
    }
}

impl std::fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

impl Serialize for ChunkId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for ChunkId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom("not a chunk id"))
    }
}

impl Manifest {
    /// Сколько байт файлов в снимке — размер мира до сжатия и дедупликации.
    pub fn source_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn chunk_ids(&self) -> impl Iterator<Item = ChunkId> + '_ {
        self.entries
            .iter()
            .flat_map(|e| e.chunks.iter().map(|c| c.id))
    }

    /// JSON под zstd с контрольной суммой кадра: битый манифест должен
    /// не разобраться, а не дать наполовину верный список файлов.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(self)?;
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        enc.include_checksum(true)?;
        std::io::Write::write_all(&mut enc, &json)?;
        Ok(enc.finish()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let json = zstd::decode_all(bytes).context("манифест не распакован")?;
        let manifest: Manifest = serde_json::from_slice(&json).context("манифест не разобран")?;
        if manifest.version != VERSION {
            bail!("манифест версии {} не поддерживается", manifest.version);
        }
        Ok(manifest)
    }
}
