//! Консоль сервера: stdout контейнера наружу, команды — в stdin.
//!
//! Строки не отправляются по одной. Стартовый стектрейс модов — это сотни
//! строк за долю секунды, и кадр на строку превращает запуск сервера в сотни
//! кадров WebSocket. Поэтому окно и потолок: то, что не поместилось, считается
//! и отдаётся числом, а не молча теряется.

use anyhow::{Context, Result};
use bollard::container::LogOutput;
use bollard::query_parameters::AttachContainerOptionsBuilder;
use futures_util::StreamExt;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::engine::Engine;

/// Окно склейки строк.
pub const FLUSH_WINDOW: Duration = Duration::from_millis(50);
/// Сколько строк максимум в одном кадре.
pub const MAX_LINES_PER_FRAME: usize = 200;
/// Признак готовности сервера — та же строка, по которой его узнаёт враппер.
pub const READY_MARKER: &str = "Done (";

pub struct ConsoleBatch {
    pub lines: Vec<String>,
    pub skipped: u32,
}

/// Присоединиться к контейнеру.
///
/// Возвращает канал батчей и ручку записи в stdin. Пока ручка жива, команды
/// уходят в сервер; когда она уронена, докер закрывает stdin, и сервер видит
/// конец ввода — поэтому её держат ровно столько, сколько живёт сервер.
pub async fn attach(
    engine: &Engine,
    server: uuid::Uuid,
) -> Result<(mpsc::Receiver<ConsoleBatch>, ConsoleWriter)> {
    let options = AttachContainerOptionsBuilder::default()
        .stream(true)
        .stdin(true)
        .stdout(true)
        .stderr(true)
        .build();

    let attached = engine
        .docker
        .attach_container(&Engine::container_name(server), Some(options))
        .await
        .context("не присоединиться к контейнеру")?;

    let (tx, rx) = mpsc::channel::<ConsoleBatch>(64);
    let mut output = attached.output;

    tokio::spawn(async move {
        let mut buffer: Vec<String> = Vec::new();
        let mut skipped: u32 = 0;
        let mut partial = String::new();
        let mut ticker = tokio::time::interval(FLUSH_WINDOW);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                chunk = output.next() => {
                    match chunk {
                        Some(Ok(out)) => {
                            partial.push_str(&decode(out));
                            // Последний кусок без перевода строки остаётся в
                            // буфере: иначе строка рвётся пополам ровно там,
                            // где докер решил отдать пакет.
                            while let Some(pos) = partial.find('\n') {
                                let line: String = partial.drain(..=pos).collect();
                                let line = line.trim_end_matches(['\n', '\r']).to_string();
                                if buffer.len() < MAX_LINES_PER_FRAME {
                                    buffer.push(line);
                                } else {
                                    skipped += 1;
                                }
                            }
                        }
                        // Контейнер закрыл поток — сервер завершился.
                        Some(Err(_)) | None => break,
                    }
                }
                _ = ticker.tick() => {
                    if buffer.is_empty() && skipped == 0 {
                        continue;
                    }
                    let batch = ConsoleBatch {
                        lines: std::mem::take(&mut buffer),
                        skipped: std::mem::take(&mut skipped),
                    };
                    if tx.send(batch).await.is_err() {
                        break;
                    }
                }
            }
        }

        if !buffer.is_empty() {
            let _ = tx
                .send(ConsoleBatch {
                    lines: buffer,
                    skipped,
                })
                .await;
        }
    });

    Ok((
        rx,
        ConsoleWriter {
            input: attached.input,
        },
    ))
}

pub struct ConsoleWriter {
    input: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
}

impl ConsoleWriter {
    /// Отправить строку так, как если бы её набрали в консоли.
    pub async fn send(&mut self, line: &str) -> Result<()> {
        let mut text = line.trim_end().to_string();
        text.push('\n');
        self.input.write_all(text.as_bytes()).await?;
        self.input.flush().await?;
        Ok(())
    }
}

fn decode(out: LogOutput) -> String {
    let bytes = match out {
        LogOutput::StdOut { message }
        | LogOutput::StdErr { message }
        | LogOutput::Console { message }
        | LogOutput::StdIn { message } => message,
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Сервер отпечатал признак готовности.
pub fn marks_ready(line: &str) -> bool {
    line.contains(READY_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ready_marker_is_the_vanilla_one() {
        assert!(marks_ready(
            r#"[12:00:00] [Server thread/INFO]: Done (21.512s)! For help, type "help""#
        ));
        assert!(!marks_ready(
            "[12:00:00] [Server thread/INFO]: Preparing spawn area: 84%"
        ));
    }
}
