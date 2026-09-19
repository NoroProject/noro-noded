//! SSH-сервер ноды: приём соединений и выдача SFTP-сессии.
//!
//! Хост-ключ генерируется один раз и хранится рядом с данными: сменившийся
//! ключ клиент показывает как «WARNING: REMOTE HOST IDENTIFICATION HAS
//! CHANGED», и человек перестаёт понимать, кому верить.

use anyhow::{Context, Result};
use russh::keys::PrivateKey;
use russh::server::{Auth, Config, Handler, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use schema::noded::NodeEvent;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::auth::{authorize, SftpGrant};
use super::handler::{split_login, SftpSession};
use crate::link::master::MasterClient;

/// Пауза после неудачной попытки. Подбор пароля становится бессмысленным, а
/// живому человеку три секунды не мешают.
const REJECTION_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct SftpServer {
    pub master: MasterClient,
    pub data_dir: PathBuf,
    pub events: mpsc::Sender<NodeEvent>,
}

impl russh::server::Server for SftpServer {
    type Handler = Connection;

    fn new_client(&mut self, _addr: Option<std::net::SocketAddr>) -> Connection {
        Connection {
            master: self.master.clone(),
            data_dir: self.data_dir.clone(),
            events: self.events.clone(),
            grant: None,
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub struct Connection {
    master: MasterClient,
    data_dir: PathBuf,
    events: mpsc::Sender<NodeEvent>,
    /// Заполняется после успешной аутентификации — из него берутся корень и права.
    grant: Option<SftpGrant>,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl Connection {
    async fn decide(&mut self, user: &str, key: Option<&str>, password: Option<&str>) -> Auth {
        if split_login(user).is_none() {
            // Логин обязан называть сервер: `игрок.адрес`. Иначе непонятно, к
            // какому из серверов человека пускать.
            return Auth::reject();
        }

        match authorize(&self.master, user, key, password).await {
            Ok(grant) => {
                self.grant = Some(grant);
                Auth::Accept
            }
            Err(e) => {
                tracing::info!(user, error = %e, "sftp: вход отклонён");
                Auth::reject()
            }
        }
    }
}

impl Handler for Connection {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let openssh = key.to_openssh().unwrap_or_default();
        Ok(self.decide(user, Some(&openssh), None).await)
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(self.decide(user, None, Some(password)).await)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    /// Только SFTP. Шелл не выдаётся: это доступ к файлам сервера, а не к
    /// машине, и выполнять на ноде чужие команды незачем.
    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        let Some(grant) = self.grant.clone() else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        let Some(channel) = self.channels.lock().await.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };

        let root = self
            .data_dir
            .join("servers")
            .join(grant.panel_server_id.to_string());
        let sftp = SftpSession::new(root, grant, self.events.clone());

        session.channel_success(channel_id)?;
        russh_sftp::server::run(channel.into_stream(), sftp).await;
        Ok(())
    }
}

/// Поднять сервер. Возвращает отпечаток хост-ключа — его показывают в панели,
/// чтобы человек сверил его, а не нажимал `yes` вслепую.
pub async fn start(server: SftpServer, bind: &str) -> Result<String> {
    let key = host_key(&server.data_dir)?;
    let fingerprint = key.public_key().fingerprint(Default::default()).to_string();

    let config = Arc::new(Config {
        auth_rejection_time: REJECTION_DELAY,
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    });

    let addr = bind.to_string();
    let mut server = server;
    tokio::spawn(async move {
        if let Err(e) = server.run_on_address(config, addr.as_str()).await {
            tracing::error!(error = %e, "sftp-сервер остановлен");
        }
    });

    Ok(fingerprint)
}

/// Хост-ключ с диска либо новый.
fn host_key(data_dir: &std::path::Path) -> Result<PrivateKey> {
    let path = data_dir.join("ssh").join("host_ed25519");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(key) = PrivateKey::from_openssh(&text) {
            return Ok(key);
        }
        tracing::warn!(path = %path.display(), "хост-ключ не разобран, создаю новый");
    }

    // `rand10`, а не воркспейсный `rand` 0.8: ssh-key собран против rand 0.10,
    // и трейты этих версий несовместимы между собой.
    let key = PrivateKey::random(&mut rand10::rng(), russh::keys::Algorithm::Ed25519)
        .context("не создать хост-ключ")?;
    std::fs::create_dir_all(path.parent().expect("у ключа есть каталог"))?;
    std::fs::write(
        &path,
        &key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?,
    )?;
    Ok(key)
}
