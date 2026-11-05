// Файл превышает 150 строк: чтение файла снимка, сборка из него tar.gz и труба в асинхронный поток — одна дорога наружу.
//! Снимок наружу: файл по кускам, tar.gz на лету и поток для HTTP и S3.
//!
//! Скачать снимок значит получить обычный `.tar.gz`, который открывает любой
//! архиватор. Собирается он по ходу отдачи, а не во временный файл: временный
//! архив двадцатигигабайтного мира — это ещё двадцать гигабайт на диске ноды
//! ровно в тот момент, когда его, может быть, и так нет.

use anyhow::Result;
use bytes::Bytes;
use flate2::write::GzEncoder;
use futures_util::Stream;
use std::io::{Read, Write};
use std::path::PathBuf;
use uuid::Uuid;

use super::manifest::{Chunk, Entry};
use super::repo::Repo;

/// Файл снимка как поток байт: куски читаются по одному и сверяются с
/// именем, в памяти лежит только текущий.
pub struct ChunkReader<'a> {
    repo: &'a Repo,
    chunks: std::slice::Iter<'a, Chunk>,
    current: std::io::Cursor<Vec<u8>>,
}

impl<'a> ChunkReader<'a> {
    pub fn new(repo: &'a Repo, entry: &'a Entry) -> Self {
        Self {
            repo,
            chunks: entry.chunks.iter(),
            current: std::io::Cursor::new(Vec::new()),
        }
    }
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.current.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            let Some(chunk) = self.chunks.next() else {
                return Ok(0);
            };
            let data = self
                .repo
                .read_chunk(chunk)
                .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
            self.current = std::io::Cursor::new(data);
        }
    }
}

/// Записать снимок как tar.gz.
///
/// Сжатие быстрое: регионы уже сжаты zlib, и уровень по умолчанию втрое
/// замедлял бы отдачу ради пары процентов.
pub fn write_tar_gz(repo: &Repo, backup: Uuid, out: impl Write) -> Result<()> {
    let manifest = repo.manifest(backup)?;
    let mut tar = tar::Builder::new(GzEncoder::new(out, flate2::Compression::fast()));
    for entry in &manifest.entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(entry.mode);
        header.set_mtime(entry.mtime.max(0) as u64);
        if entry.dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            tar.append_data(&mut header, &entry.path, std::io::empty())?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.size);
            tar.append_data(&mut header, &entry.path, ChunkReader::new(repo, entry))?;
        }
    }
    tar.into_inner()?.finish()?.flush()?;
    Ok(())
}

/// Сколько байт копится перед отправкой в поток. Мельче — лишние переключения
/// между потоками, крупнее — лишняя память на каждое скачивание.
const BATCH: usize = 256 * 1024;

/// tar.gz снимка асинхронным потоком. Собирается в блокирующем пуле; закрытый
/// получатель (браузер ушёл) обрывает сборку на следующей записи.
pub fn tar_gz_stream(
    repo_dir: PathBuf,
    backup: Uuid,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(8);
    tokio::task::spawn_blocking(move || {
        let repo = Repo::at(&repo_dir);
        let mut pipe = Pipe {
            tx: tx.clone(),
            buf: Vec::with_capacity(BATCH),
        };
        let done = write_tar_gz(&repo, backup, &mut pipe).and_then(|_| Ok(pipe.flush()?));
        if let Err(e) = done {
            tracing::warn!(%backup, error = %format!("{e:#}"), "снимок не отдан");
            let _ = tx.blocking_send(Err(std::io::Error::other(format!("{e:#}"))));
        }
    });
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

struct Pipe {
    tx: tokio::sync::mpsc::Sender<std::io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl Write for Pipe {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= BATCH {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let batch = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(BATCH)));
        self.tx
            .blocking_send(Ok(batch))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "получатель ушёл"))
    }
}

#[cfg(all(test, unix))]
#[path = "stream_tests.rs"]
mod tests;
