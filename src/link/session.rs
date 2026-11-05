// Файл превышает 150 строк: одна сессия — это подключение, hello, две петли и разбор ответов; врозь их не прочитать.
//! Исходящее соединение с мастером.
//!
//! Звонит нода, а не мастер: так она работает за NAT, без белого адреса,
//! домена и сертификата. Переподключение — с нарастающей паузой и джиттером:
//! десяток нод, потерявших мастер одновременно, иначе вернутся к нему ровно в
//! один и тот же момент.
//!
//! Операции выполняются **не в петле чтения**, а задачами, и отвечают через
//! общий канал. Установка идёт минутами, бэкап большого мира — до четверти
//! часа: делая их прямо здесь, нода на всё это время переставала слать консоль
//! и состояния, то есть выглядела зависшей ровно тогда, когда на неё смотрят.

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
/// Сколько ждать рукопожатия и отправки одного кадра.
///
/// Без потолка зависшее рукопожатие (TCP принят, TLS молчит) держало ноду
/// неподключённой навсегда, а отправка в полуоткрытое соединение висела, пока
/// ядро не сдастся через четверть часа: петля стояла на ней и даже не
/// проверяла, что мастер давно молчит.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Сколько может занять опрос состояний: зависший докер не должен держать
/// петлю, которая ещё и читает кадры мастера.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(daemon: Daemon, mut events: mpsc::Receiver<NodeEvent>) -> ! {
    let mut backoff = BACKOFF_START;
    loop {
        let result = connect_once(&daemon, &mut events).await;
        daemon.health.disconnected();
        match result {
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

    let (stream, _) = tokio::time::timeout(IO_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .context("мастер не ответил на подключение")?
        .context("не подключиться к мастеру")?;
    tracing::info!(url = %daemon.cfg.ws_url(), "подключён к мастеру");
    let (mut tx, mut rx) = stream.split();

    // Первым кадром — кто мы и что у нас уже крутится. Мастер сверит это со
    // своей таблицей: контейнер, снятый руками, и сервер, заведённый пока нода
    // была в офлайне, обнаруживаются только здесь.
    let hello = hello(daemon).await?;
    push(&mut tx, Message::Text(serde_json::to_string(&hello)?)).await?;
    daemon.health.connected();

    let mut poll = tokio::time::interval(STATE_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = tokio::time::Instant::now();

    // Ответы задач возвращаются сюда: писать в сокет из нескольких задач
    // нельзя, а отдавать им сам сокет — тем более.
    let (replies_tx, mut replies) = mpsc::unbounded_channel::<String>();

    loop {
        tokio::select! {
            incoming = rx.next() => {
                let Some(message) = incoming else { return Ok(()) };
                match message? {
                    Message::Text(text) => {
                        last_seen = tokio::time::Instant::now();
                        daemon.health.touch();
                        handle_frame(daemon, &replies_tx, &text);
                    }
                    Message::Ping(payload) => {
                        push(&mut tx, Message::Pong(payload)).await?;
                        last_seen = tokio::time::Instant::now();
                        daemon.health.touch();
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }

            Some(frame) = replies.recv() => {
                push(&mut tx, Message::Text(frame)).await?;
            }

            Some(event) = events.recv() => {
                let frame = FromNode::Event { event };
                push(&mut tx, Message::Text(serde_json::to_string(&frame)?)).await?;
            }

            _ = poll.tick() => {
                // Тишина дольше окна — соединение полуоткрыто: такое рвётся
                // молча и съедает команды, которые мастер считает доставленными.
                if last_seen.elapsed() > OFFLINE_AFTER {
                    bail!("мастер молчит дольше {OFFLINE_AFTER:?}");
                }
                let polled = tokio::time::timeout(
                    POLL_TIMEOUT,
                    dispatch::poll_states(&daemon.engine, &daemon.registry),
                )
                .await;
                let Ok(states) = polled else {
                    tracing::warn!("докер не ответил о состояниях вовремя");
                    continue;
                };
                for event in states {
                    let frame = FromNode::Event { event };
                    push(&mut tx, Message::Text(serde_json::to_string(&frame)?)).await?;
                }
            }
        }
    }
}

/// Отправить кадр, не дольше `IO_TIMEOUT`.
async fn push<S>(tx: &mut S, message: Message) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    tokio::time::timeout(IO_TIMEOUT, tx.send(message))
        .await
        .context("мастер не принимает кадры")??;
    Ok(())
}

/// Разобрать кадр мастера. Ответ уходит каналом, а не в сокет: писать в него
/// имеет право только петля, иначе два кадра перемешаются на полуслове.
fn handle_frame(daemon: &Daemon, replies: &mpsc::UnboundedSender<String>, text: &str) {
    let frame: ToNode = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            // Незнакомый кадр — это мастер новее ноды. Рвать из-за этого связь
            // нельзя: остальные операции работают.
            tracing::warn!(error = %e, "кадр от мастера не разобран");
            return;
        }
    };

    match frame {
        ToNode::Ping { seq } => send(replies, &FromNode::Pong { seq }),
        ToNode::Cancel { id } => tracing::info!(id, "отмена запроса"),
        ToNode::Request { id, op } => {
            let daemon = daemon.clone();
            let replies = replies.clone();
            tokio::spawn(async move {
                let frame = match dispatch::handle(&daemon, op).await {
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
                send(&replies, &frame);
            });
        }
    }
}

/// Отказ отправки значит «соединение уже закрыто»: задача переживает разрыв,
/// а мастер после переподключения спросит заново.
fn send(replies: &mpsc::UnboundedSender<String>, frame: &FromNode) {
    match serde_json::to_string(frame) {
        Ok(text) => {
            let _ = replies.send(text);
        }
        Err(e) => tracing::warn!(error = %e, "ответ не сериализован"),
    }
}

async fn hello(daemon: &Daemon) -> Result<FromNode> {
    let docker_version = daemon.engine.version().await.unwrap_or_default();
    let servers = daemon.engine.snapshot().await.unwrap_or_default();
    let machine = crate::metrics::machine(&daemon.cfg.data_dir);

    let mut capabilities = vec![
        caps::IMAGE_PULL.to_string(),
        caps::BACKUP_PARTIAL.to_string(),
    ];
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
