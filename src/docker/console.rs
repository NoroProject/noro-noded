// Файл превышает 150 строк: чтение потока, нарезка на строки и склейка в кадры — одна труба, и врозь её не проследить.
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
/// Самая длинная строка, которая уходит целиком. Строку без перевода, которую
/// печатает сломанный мод или бинарный мусор в stdout, демон иначе копил в
/// памяти без предела.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

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
        let mut splitter = LineSplitter::default();
        let mut lines = Vec::new();
        let mut ticker = tokio::time::interval(FLUSH_WINDOW);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                chunk = output.next() => {
                    match chunk {
                        Some(Ok(out)) => {
                            splitter.feed(&payload(out), &mut lines);
                            for line in lines.drain(..) {
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

fn payload(out: LogOutput) -> bytes::Bytes {
    match out {
        LogOutput::StdOut { message }
        | LogOutput::StdErr { message }
        | LogOutput::Console { message }
        | LogOutput::StdIn { message } => message,
    }
}

/// Нарезка потока байт на строки.
///
/// Режется по байту перевода строки, а в текст превращается только целая
/// строка: докер отдаёт пакеты где придётся, и буква кириллицы, разрезанная
/// между двумя пакетами, при раздельном разборе становилась парой `�`.
///
/// Последний кусок без перевода строки остаётся в буфере до следующего пакета,
/// но не длиннее `MAX_LINE_BYTES`: хвост сверх него отбрасывается до конца
/// строки, а сама строка помечается обрезанной.
#[derive(Default)]
struct LineSplitter {
    partial: Vec<u8>,
    cut: bool,
}

impl LineSplitter {
    fn feed(&mut self, mut bytes: &[u8], out: &mut Vec<String>) {
        while let Some(pos) = bytes.iter().position(|b| *b == b'\n') {
            self.append(&bytes[..pos]);
            out.push(self.take_line());
            bytes = &bytes[pos + 1..];
        }
        self.append(bytes);
    }

    fn append(&mut self, bytes: &[u8]) {
        let room = MAX_LINE_BYTES.saturating_sub(self.partial.len());
        if bytes.len() > room {
            self.partial.extend_from_slice(&bytes[..room]);
            self.cut = true;
        } else {
            self.partial.extend_from_slice(bytes);
        }
    }

    fn take_line(&mut self) -> String {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        let mut line = String::from_utf8_lossy(&self.partial).into_owned();
        if std::mem::take(&mut self.cut) {
            line.push_str(" …");
        }
        self.partial.clear();
        line
    }
}

/// Сервер отпечатал признак готовности.
pub fn marks_ready(line: &str) -> bool {
    line.contains(READY_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            splitter.feed(chunk, &mut out);
        }
        out
    }

    /// «Ж» — два байта; докер вправе отдать их разными пакетами.
    #[test]
    fn a_letter_split_between_packets_stays_whole() {
        let text = "Привет\n".as_bytes();
        assert_eq!(split(&[&text[..3], &text[3..]]), vec!["Привет"]);
    }

    #[test]
    fn crlf_and_unfinished_lines_are_handled() {
        assert_eq!(split(&[b"a\r\nb", b"c\n", b"tail"]), vec!["a", "bc"]);
    }

    #[test]
    fn an_endless_line_is_cut_and_does_not_grow() {
        let mut splitter = LineSplitter::default();
        let mut out = Vec::new();
        let junk = vec![b'x'; MAX_LINE_BYTES];
        for _ in 0..10 {
            splitter.feed(&junk, &mut out);
        }
        assert!(splitter.partial.len() <= MAX_LINE_BYTES, "буфер не растёт");
        splitter.feed(b"\nnext\n", &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with(" …"), "обрезанная строка помечена");
        assert_eq!(out[1], "next");
    }

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
