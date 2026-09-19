//! Server list ping: ask a running server whether anybody is on it.
//!
//! The same request a client sends before you click a server in the list. It
//! works on any Minecraft server, with our agent or without it, which is what
//! makes it the fallback source of "is anybody playing" — and the only one for
//! a server running somebody else's plugins.
//!
//! Only the handshake and the status request are implemented: this is a
//! question with a JSON answer, not a protocol client.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Protocol version in the handshake.
///
/// `-1` means "I am only asking": servers answer a status request from any
/// version, and naming a real one would make an old server refuse a new client.
const PROTOCOL_ANY: i32 = -1;

/// A server busy saving a large world answers late. Longer than this and the
/// answer is useless anyway — we are asking to decide whether to keep it up.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the answer. A well-behaved server sends a few kilobytes; the limit is
/// here so a broken one cannot make the daemon read forever.
const MAX_RESPONSE: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Pong {
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub players_online: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub players_max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

impl Pong {
    /// Server did not answer. Not an error: a stopped server is a normal state,
    /// and the caller wants the answer, not an exception.
    pub fn silent() -> Self {
        Self {
            online: false,
            motd: None,
            players_online: None,
            players_max: None,
            version: None,
            latency_ms: None,
        }
    }
}

pub async fn ping(host: &str, port: u16) -> Pong {
    match tokio::time::timeout(TIMEOUT, ask(host, port)).await {
        Ok(Ok(pong)) => pong,
        // Both a refused connection and a timeout mean the same thing here:
        // nobody answered.
        Ok(Err(e)) => {
            tracing::debug!(host, port, error = %e, "сервер не ответил на пинг");
            Pong::silent()
        }
        Err(_) => Pong::silent(),
    }
}

async fn ask(host: &str, port: u16) -> Result<Pong> {
    let started = Instant::now();
    let mut stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;

    let mut handshake = Vec::new();
    write_varint(&mut handshake, 0x00); // packet id: handshake
    write_varint(&mut handshake, PROTOCOL_ANY);
    write_string(&mut handshake, host);
    handshake.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut handshake, 1); // next state: status

    send_packet(&mut stream, &handshake).await?;
    send_packet(&mut stream, &[0x00]).await?; // status request

    let body = read_packet(&mut stream).await?;
    let mut cursor = &body[..];
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != 0x00 {
        bail!("unexpected packet id in status response: {packet_id}");
    }

    let json = read_string(&mut cursor)?;
    Ok(parse(&json, started.elapsed().as_millis() as u64))
}

fn parse(json: &str, latency_ms: u64) -> Pong {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        // Answered, but with something we do not understand: the server is up,
        // and that is the part the caller acts on.
        Err(_) => {
            return Pong {
                online: true,
                latency_ms: Some(latency_ms),
                ..Pong::silent()
            }
        }
    };

    Pong {
        online: true,
        motd: motd_of(&value["description"]),
        players_online: value["players"]["online"].as_i64(),
        players_max: value["players"]["max"].as_i64(),
        version: value["version"]["name"].as_str().map(str::to_owned),
        latency_ms: Some(latency_ms),
    }
}

/// The description is a chat component: a plain string on older servers, an
/// object with `text` and `extra` on newer ones. Only the text is taken —
/// colours belong to the client that draws it.
fn motd_of(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }

    let mut out = String::new();
    collect_text(value, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn collect_text(value: &serde_json::Value, out: &mut String) {
    if let Some(text) = value["text"].as_str() {
        out.push_str(text);
    }
    if let Some(extra) = value["extra"].as_array() {
        for part in extra {
            if let Some(text) = part.as_str() {
                out.push_str(text);
            } else {
                collect_text(part, out);
            }
        }
    }
}

// --- wire format ---------------------------------------------------------

async fn send_packet(stream: &mut TcpStream, body: &[u8]) -> Result<()> {
    let mut framed = Vec::with_capacity(body.len() + 5);
    write_varint(&mut framed, body.len() as i32);
    framed.extend_from_slice(body);
    stream.write_all(&framed).await?;
    Ok(())
}

async fn read_packet(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let len = read_varint_async(stream).await?;
    if len <= 0 || len as usize > MAX_RESPONSE {
        bail!("status response of {len} bytes is out of bounds");
    }

    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn read_varint_async(stream: &mut TcpStream) -> Result<i32> {
    let mut value = 0i32;
    for shift in 0..5 {
        let byte = stream.read_u8().await?;
        value |= ((byte & 0x7F) as i32) << (shift * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint longer than five bytes")
}

pub(crate) fn write_varint(out: &mut Vec<u8>, mut value: i32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        // Arithmetic shift would keep the sign bits and loop forever on a
        // negative number — and the handshake sends exactly one.
        value = ((value as u32) >> 7) as i32;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

pub(crate) fn read_varint(input: &mut &[u8]) -> Result<i32> {
    let mut value = 0i32;
    for shift in 0..5 {
        let (byte, rest) = input.split_first().context("varint cut short")?;
        *input = rest;
        value |= ((byte & 0x7F) as i32) << (shift * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("varint longer than five bytes")
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_varint(out, value.len() as i32);
    out.extend_from_slice(value.as_bytes());
}

fn read_string(input: &mut &[u8]) -> Result<String> {
    let len = read_varint(input)? as usize;
    if len > input.len() {
        bail!("string longer than the packet");
    }
    let (text, rest) = input.split_at(len);
    *input = rest;
    Ok(String::from_utf8(text.to_vec())?)
}

#[cfg(test)]
#[path = "ping_tests.rs"]
mod tests;
