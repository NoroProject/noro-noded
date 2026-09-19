// Файл превышает 150 строк: одна сессия — это подключение, hello, две петли и разбор ответов; врозь их не прочитать.
//! Исходящее соединение с мастером.
//!
//! Звонит нода, а не мастер: так она работает за NAT, без белого адреса,
//! домена и сертификата. Переподключение — с нарастающей паузой и джиттером:
//! десяток нод, потерявших мастер одновременно, иначе вернутся к нему ровно в
//! один и тот же момент.

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use schema::noded::{caps, FromNode, NodeEvent, NodeHello, ToNode, OFFLINE_AFTER};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::link::dispatch;
use crate::Daemon;

const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Как часто нода сама отчитывается о состоянии серверов.
const STATE_POLL: Duration = Duration::from_secs(20);

pub async fn run(daemon: Daemon, mut events: mpsc::Receiver<NodeEvent>) -> ! {
    let mut backoff = BACKOFF_START;
    loop {
        match connect_once(&daemon, &mut events).await {
            Ok(()) => {
                tracing::warn!("мастер закрыл соединение");
                backoff = BACKOFF_START;
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "соединение с мастером не держится")
            }
        }

        let jitter = Duration::from_millis(rand::random::<u64>() % 1000);
        tokio::time::sleep(backoff + jitter).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn connect_once(daemon: &Daemon, events: &mut mpsc::Receiver<NodeEvent>) -> Result<()> {
    let mut request = daemon.cfg.ws_url().into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", daemon.cfg.token).parse()?,
    );

    let (stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("не подключиться к мастеру")?;
    tracing::info!(url = %daemon.cfg.ws_url(), "подключён к мастеру");
    let (mut tx, mut rx) = stream.split();

    // Первым кадром — кто мы и что у нас уже крутится. Мастер сверит это со
    // своей таблицей: контейнер, снятый руками, и сервер, заведённый пока нода
    // была в офлайне, обнаруживаются только здесь.
    let hello = hello(daemon).await?;
    tx.send(Message::Text(serde_json::to_string(&hello)?))
        .await?;

    let mut poll = tokio::time::interval(STATE_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = tokio::time::Instant::now();

    loop {
        tokio::select! {
            incoming = rx.next() => {
                let Some(message) = incoming else { return Ok(()) };
                match message? {
                    Message::Text(text) => {
                        last_seen = tokio::time::Instant::now();
                        handle_frame(daemon, &mut tx, &text).await?;
                    }
                    Message::Ping(payload) => {
                        tx.send(Message::Pong(payload)).await?;
                        last_seen = tokio::time::Instant::now();
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }

            Some(event) = events.recv() => {
                let frame = FromNode::Event { event };
                tx.send(Message::Text(serde_json::to_string(&frame)?)).await?;
            }

            _ = poll.tick() => {
                // Тишина дольше окна — соединение полуоткрыто: такое рвётся
                // молча и съедает команды, которые мастер считает доставленными.
                if last_seen.elapsed() > OFFLINE_AFTER {
                    bail!("мастер молчит дольше {OFFLINE_AFTER:?}");
                }
                for event in dispatch::poll_states(&daemon.engine, &daemon.registry).await {
                    let frame = FromNode::Event { event };
                    tx.send(Message::Text(serde_json::to_string(&frame)?)).await?;
                }
            }
        }
    }
}

type Sink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

async fn handle_frame(daemon: &Daemon, tx: &mut Sink, text: &str) -> Result<()> {
    let frame: ToNode = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            // Незнакомый кадр — это мастер новее ноды. Рвать из-за этого связь
            // нельзя: остальные операции работают.
            tracing::warn!(error = %e, "кадр от мастера не разобран");
            return Ok(());
        }
    };

    match frame {
        ToNode::Ping { seq } => {
            let pong = FromNode::Pong { seq };
            tx.send(Message::Text(serde_json::to_string(&pong)?))
                .await?;
        }
        ToNode::Cancel { id } => {
            tracing::info!(id, "отмена запроса");
        }
        ToNode::Request { id, op } => {
            let daemon = daemon.clone();
            let reply = dispatch::handle(&daemon, op).await;
            let frame = match reply {
                Ok(data) => FromNode::Reply {
                    id,
                    ok: true,
                    data,
                    error: None,
                },
                Err(error) => FromNode::Reply {
                    id,
                    ok: false,
                    data: serde_json::Value::Null,
                    error: Some(error),
                },
            };
            tx.send(Message::Text(serde_json::to_string(&frame)?))
                .await?;
        }
    }
    Ok(())
}

async fn hello(daemon: &Daemon) -> Result<FromNode> {
    let docker_version = daemon.engine.version().await.unwrap_or_default();
    let servers = daemon.engine.snapshot().await.unwrap_or_default();
    let machine = crate::metrics::machine();

    let mut capabilities = vec![caps::IMAGE_PULL.to_string()];
    if daemon.cfg.http.public_url.is_some() {
        capabilities.push(caps::DIRECT_TRANSFER.to_string());
    }
    if daemon.cfg.sftp.enabled {
        capabilities.push(caps::SFTP.to_string());
    }

    Ok(FromNode::Hello {
        info: NodeHello {
            node_version: env!("CARGO_PKG_VERSION").to_string(),
            docker_version,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpus: machine.cpus,
            total_memory_mb: machine.memory_mb,
            total_disk_mb: machine.disk_mb,
            capabilities,
            public_url: daemon.cfg.http.public_url.clone(),
            sftp_port: daemon.cfg.sftp.enabled.then(|| daemon.sftp_port()),
            sftp_fingerprint: daemon.sftp_fingerprint.lock().clone(),
            servers,
        },
    })
}
