// Over 150 lines: the placeholder answers two different questions on one port, and they only make sense together.
//! Hold the port of a sleeping server and wake it when somebody knocks.
//!
//! Without this, a server that went to sleep is simply offline: the player sees
//! a red cross in the list and no way to do anything about it. With it, the
//! sleeping server still answers the server list — "sleeping, join to start" —
//! and the first login attempt starts the container.
//!
//! The listener speaks just enough of the protocol to answer those two
//! questions. It is not a proxy: the connection is never forwarded, it is
//! answered and closed. Forwarding would mean holding client traffic while the
//! server boots, and a player staring at a frozen screen for two minutes is
//! worse than a clear "come back in a moment".

use anyhow::{bail, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use uuid::Uuid;

use schema::noded::Listing;

use super::ping::{read_varint, write_varint};

/// Next state in the handshake.
const STATE_STATUS: i32 = 1;
const STATE_LOGIN: i32 = 2;

/// A knock on the port, and the handshake around it, should take milliseconds.
/// Anything slower is a scanner, not a player.
const CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A handle on the port held for a sleeping server.
pub struct Placeholder {
    /// One-shot rather than a `Notify`: `notify_waiters` only reaches somebody
    /// already waiting, and releasing a placeholder the instant after starting
    /// it left the port held forever.
    stop: Option<oneshot::Sender<()>>,
}

impl Placeholder {
    /// Stop holding the port. Called before the container starts: the server
    /// needs the port itself, and two listeners on it is one refusal.
    pub fn release(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for Placeholder {
    /// Dropping the handle releases the port too: a placeholder nobody holds
    /// on to is a port nobody can take back.
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// What the placeholder does when somebody tries to log in.
pub type WakeFn = Arc<dyn Fn(Uuid) + Send + Sync>;

/// Start holding `port` for `server`.
///
/// `wake` is called once, on the first login attempt. Everything after that is
/// the caller's problem: the placeholder stops listening and goes away.
pub async fn hold(server: Uuid, port: u16, listing: Listing, wake: WakeFn) -> Result<Placeholder> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let (stop, mut stop_signal) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_signal => {
                    tracing::debug!(%server, port, "порт отпущен");
                    return;
                }
                accepted = listener.accept() => {
                    let Ok((stream, addr)) = accepted else { continue };
                    let listing = listing.clone();
                    let wake = wake.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            CLIENT_TIMEOUT,
                            serve(stream, addr, &listing, server, wake),
                        )
                        .await
                        {
                            Ok(Err(e)) => tracing::debug!(%server, error = %e, "стук не разобран"),
                            Err(_) => tracing::debug!(%server, %addr, "стук без продолжения"),
                            Ok(Ok(())) => {}
                        }
                    });
                }
            }
        }
    });

    Ok(Placeholder { stop: Some(stop) })
}

async fn serve(
    mut stream: TcpStream,
    addr: SocketAddr,
    listing: &Listing,
    server: Uuid,
    wake: WakeFn,
) -> Result<()> {
    let handshake = read_packet(&mut stream).await?;
    let next_state = parse_handshake(&handshake)?;

    match next_state {
        STATE_STATUS => {
            answer_status(&mut stream, listing).await?;
            // The client sends a ping right after the status and waits for it
            // back; without the echo the server list shows "no connection"
            // instead of the motd we just sent.
            if let Ok(ping) = read_packet(&mut stream).await {
                let _ = stream.write_all(&framed(&ping)).await;
            }
            Ok(())
        }
        STATE_LOGIN => {
            tracing::info!(%server, %addr, "сервер будят подключением");
            wake(server);
            // The disconnect reason is the only way to say anything to somebody
            // already on the login screen.
            answer_login_refusal(&mut stream, &listing.motd).await
        }
        other => bail!("unexpected next state in handshake: {other}"),
    }
}

/// Only the last field of the handshake matters: what the client wants next.
fn parse_handshake(body: &[u8]) -> Result<i32> {
    let mut cursor = body;
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != 0x00 {
        bail!("first packet is not a handshake: {packet_id}");
    }

    let _protocol = read_varint(&mut cursor)?;
    let host_len = read_varint(&mut cursor)? as usize;
    if host_len > cursor.len() {
        bail!("handshake host longer than the packet");
    }
    cursor = &cursor[host_len..];
    if cursor.len() < 2 {
        bail!("handshake cut short before the port");
    }
    cursor = &cursor[2..];

    read_varint(&mut cursor)
}

async fn answer_status(stream: &mut TcpStream, listing: &Listing) -> Result<()> {
    // The version line is drawn by the client only when the protocol does not
    // match — and here it never does, on purpose: the one answering is not a
    // server, and saying "1.21.1" would be a lie the client then acts on.
    let version = if listing.version_name.is_empty() {
        listing.motd.as_str()
    } else {
        listing.version_name.as_str()
    };

    let mut status = serde_json::json!({
        "version": { "name": version, "protocol": listing.protocol },
        "players": { "online": 0, "max": 0, "sample": [] },
        "description": { "text": listing.motd },
    });
    // The icon is sent as the protocol wants it — a data URI. The master
    // prepares it; the node does not resize or re-encode anything.
    if let Some(favicon) = &listing.favicon {
        status["favicon"] = serde_json::Value::String(favicon.clone());
    }

    let json = status.to_string();

    let mut body = Vec::new();
    write_varint(&mut body, 0x00);
    write_varint(&mut body, json.len() as i32);
    body.extend_from_slice(json.as_bytes());

    stream.write_all(&framed(&body)).await?;
    Ok(())
}

async fn answer_login_refusal(stream: &mut TcpStream, motd: &str) -> Result<()> {
    let reason = serde_json::json!({ "text": motd }).to_string();

    let mut body = Vec::new();
    write_varint(&mut body, 0x00); // login disconnect
    write_varint(&mut body, reason.len() as i32);
    body.extend_from_slice(reason.as_bytes());

    stream.write_all(&framed(&body)).await?;
    Ok(())
}

pub(crate) fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    write_varint(&mut out, body.len() as i32);
    out.extend_from_slice(body);
    out
}

async fn read_packet(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = 0i32;
    for shift in 0..5 {
        let byte = stream.read_u8().await?;
        len |= ((byte & 0x7F) as i32) << (shift * 7);
        if byte & 0x80 == 0 {
            break;
        }
    }
    // A handshake is tens of bytes. The cap is here so a hostile client cannot
    // make the daemon allocate whatever it names.
    if len <= 0 || len > 32 * 1024 {
        bail!("packet of {len} bytes is out of bounds");
    }

    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
#[path = "waker_tests.rs"]
mod tests;
