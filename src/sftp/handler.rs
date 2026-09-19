// Файл превышает 150 строк: SFTP-хендлер — это таблица операций протокола, и порознь она не читается.
//! Операции SFTP поверх каталога сервера.
//!
//! Все пути идут через тот же `fs::safe`, что и HTTP-операции: одна реализация
//! на две точки входа. Права те же, что выдал мастер, — запрет на запись здесь
//! означает `SSH_FX_PERMISSION_DENIED`, а не молчаливое игнорирование.

use russh_sftp::protocol::{
    Attrs, File, FileAttributes, Handle, Name, Status, StatusCode, Version,
};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

use super::auth::SftpGrant;
use schema::noded::{NodeEvent, SftpActionKind};

pub struct SftpSession {
    pub root: PathBuf,
    pub grant: SftpGrant,
    pub events: mpsc::Sender<NodeEvent>,
    /// Открытые файлы и каталоги: протокол адресует их строкой-ручкой.
    files: HashMap<String, std::fs::File>,
    dirs: HashMap<String, Vec<File>>,
    version: Option<u32>,
}

impl SftpSession {
    pub fn new(root: PathBuf, grant: SftpGrant, events: mpsc::Sender<NodeEvent>) -> Self {
        Self {
            root,
            grant,
            events,
            files: HashMap::new(),
            dirs: HashMap::new(),
            version: None,
        }
    }

    /// Путь внутри корня либо отказ. Единственная дверь наружу.
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let rel = path.trim_start_matches('/');
        if crate::fs::is_hidden(rel) {
            return Err(StatusCode::PermissionDenied);
        }
        crate::fs::resolve(&self.root, rel).map_err(|_| StatusCode::PermissionDenied)
    }

    fn require_write(&self) -> Result<(), StatusCode> {
        if self.grant.can_write {
            Ok(())
        } else {
            Err(StatusCode::PermissionDenied)
        }
    }

    fn require_delete(&self) -> Result<(), StatusCode> {
        if self.grant.can_delete {
            Ok(())
        } else {
            Err(StatusCode::PermissionDenied)
        }
    }

    /// Что сделали через SFTP — в журнал сервера: владелец должен видеть и то,
    /// что пришло мимо браузера.
    fn report(&self, action: SftpActionKind, path: &str) {
        let event = NodeEvent::SftpAction {
            server: self.grant.panel_server_id,
            user_id: self.grant.user_id,
            action,
            path: path.to_string(),
        };
        let events = self.events.clone();
        tokio::spawn(async move {
            let _ = events.send(event).await;
        });
    }

    fn ok(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        }
    }
}

fn attrs_of(meta: &std::fs::Metadata) -> FileAttributes {
    let mut attrs = FileAttributes {
        size: Some(meta.len()),
        ..Default::default()
    };
    attrs.set_dir(meta.is_dir());
    attrs.set_regular(meta.is_file());
    if let Ok(modified) = meta.modified() {
        if let Ok(secs) = modified.duration_since(std::time::UNIX_EPOCH) {
            attrs.mtime = Some(secs.as_secs() as u32);
            attrs.atime = Some(secs.as_secs() as u32);
        }
    }
    attrs
}

impl russh_sftp::server::Handler for SftpSession {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        if self.version.is_some() {
            return Err(StatusCode::ConnectionLost);
        }
        self.version = Some(version);
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        // Клиент спрашивает канонический путь ещё до всякой работы; отдаём
        // относительный, потому что корнем для него является корень сервера.
        let cleaned = if path == "." || path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", path.trim_start_matches('/'))
        };
        Ok(Name {
            id,
            files: vec![File::dummy(&cleaned)],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let dir = self.resolve(&path)?;
        let listing = std::fs::read_dir(&dir).map_err(|_| StatusCode::NoSuchFile)?;

        let mut files = Vec::new();
        for entry in listing.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = format!("{}/{name}", path.trim_end_matches('/'));
            if crate::fs::is_hidden(child.trim_start_matches('/')) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            files.push(File::new(&name, attrs_of(&meta)));
        }

        self.dirs.insert(path.clone(), files);
        Ok(Handle { id, handle: path })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        // Каталог отдаётся одним ответом, дальше — EOF: иначе клиент будет
        // спрашивать его по кругу.
        match self.dirs.remove(&handle) {
            Some(files) => Ok(Name { id, files }),
            None => Err(StatusCode::Eof),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: russh_sftp::protocol::OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let writing = pflags.contains(russh_sftp::protocol::OpenFlags::WRITE)
            || pflags.contains(russh_sftp::protocol::OpenFlags::CREATE)
            || pflags.contains(russh_sftp::protocol::OpenFlags::TRUNCATE);
        if writing {
            self.require_write()?;
        }

        let path = self.resolve(&filename)?;
        let file = std::fs::OpenOptions::new()
            .read(pflags.contains(russh_sftp::protocol::OpenFlags::READ))
            .write(writing)
            .create(pflags.contains(russh_sftp::protocol::OpenFlags::CREATE))
            .truncate(pflags.contains(russh_sftp::protocol::OpenFlags::TRUNCATE))
            .open(&path)
            .map_err(|_| StatusCode::NoSuchFile)?;

        if writing {
            self.report(SftpActionKind::Write, &filename);
        }
        self.files.insert(filename.clone(), file);
        Ok(Handle {
            id,
            handle: filename,
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<russh_sftp::protocol::Data, Self::Error> {
        use std::io::{Read, Seek, SeekFrom};

        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;

        let mut buf = vec![0u8; len as usize];
        let read = file.read(&mut buf).map_err(|_| StatusCode::Failure)?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(read);
        Ok(russh_sftp::protocol::Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        use std::io::{Seek, SeekFrom, Write};

        self.require_write()?;
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        file.write_all(&data).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.files.remove(&handle);
        self.dirs.remove(&handle);
        Ok(Self::ok(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, path).await
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let resolved = self.resolve(&path)?;
        let meta = std::fs::metadata(&resolved).map_err(|_| StatusCode::NoSuchFile)?;
        Ok(Attrs {
            id,
            attrs: attrs_of(&meta),
        })
    }

    /// Ручка здесь — тот же путь, что и при открытии: протокол адресует
    /// открытый файл строкой, и своей таблицы дескрипторов заводить незачем.
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, handle).await
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.require_write()?;
        let dir = self.resolve(&path)?;
        std::fs::create_dir_all(dir).map_err(|_| StatusCode::Failure)?;
        self.report(SftpActionKind::Mkdir, &path);
        Ok(Self::ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.require_delete()?;
        let path = self.resolve(&filename)?;
        std::fs::remove_file(path).map_err(|_| StatusCode::Failure)?;
        self.report(SftpActionKind::Delete, &filename);
        Ok(Self::ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.require_delete()?;
        let dir = self.resolve(&path)?;
        std::fs::remove_dir_all(dir).map_err(|_| StatusCode::Failure)?;
        self.report(SftpActionKind::Delete, &path);
        Ok(Self::ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.require_write()?;
        let from = self.resolve(&oldpath)?;
        let to = self.resolve(&newpath)?;
        std::fs::rename(from, to).map_err(|_| StatusCode::Failure)?;
        self.report(SftpActionKind::Rename, &newpath);
        Ok(Self::ok(id))
    }

    /// Симлинки через SFTP не создаются вовсе.
    ///
    /// Ссылка наружу — это побег, а проверять её здесь пришлось бы второй
    /// реализацией того, что уже делает разрешатель путей. Дешевле не уметь.
    async fn symlink(
        &mut self,
        _id: u32,
        _linkpath: String,
        _targetpath: String,
    ) -> Result<Status, Self::Error> {
        Err(StatusCode::OpUnsupported)
    }
}

/// Идентификатор сервера из логина `игрок.адрес`.
pub fn split_login(login: &str) -> Option<(&str, &str)> {
    let (user, slug) = login.rsplit_once('.')?;
    if user.is_empty() || slug.is_empty() {
        return None;
    }
    Some((user, slug))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_names_the_player_and_the_server() {
        assert_eq!(split_login("alice.survival"), Some(("alice", "survival")));
        // Ник с точкой тоже бывает: сервер — то, что после **последней** точки.
        assert_eq!(split_login("a.b.test-town"), Some(("a.b", "test-town")));
        assert_eq!(split_login("alice"), None);
        assert_eq!(split_login(".survival"), None);
        assert_eq!(split_login("alice."), None);
    }

    /// Уникальный идентификатор сессии не участвует в путях — проверка, что
    /// корень действительно приходит снаружи, а не собирается из логина.
    #[test]
    fn the_session_root_is_whatever_the_master_said() {
        let grant = SftpGrant {
            user_id: uuid::Uuid::nil(),
            panel_server_id: uuid::Uuid::nil(),
            can_write: false,
            can_delete: false,
        };
        let (tx, _rx) = mpsc::channel(1);
        let session = SftpSession::new(PathBuf::from("/srv/x"), grant, tx);
        assert_eq!(session.root, PathBuf::from("/srv/x"));
        assert!(session.require_write().is_err(), "без права записи — отказ");
        assert!(session.require_delete().is_err());
    }
}
