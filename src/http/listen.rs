//! Приём соединений прямой передачи с тайм-аутом на заголовки.
//!
//! Порт смотрит в интернет, а `axum::serve` таймер hyper не ставит — и
//! тайм-аут чтения заголовков без таймера не действует. Соединение, которое
//! присылает заголовок по байту в минуту, держало задачу и сокет демона сколько
//! угодно, и тысяча таких занимала все дескрипторы ноды. Поэтому соединения
//! здесь обслуживаются вручную, той же связкой hyper, но с таймером.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};

/// Сколько клиент может присылать заголовки запроса. Браузеру хватает долей
/// секунды; тело (сама заливка) этим не ограничено.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Обслуживать соединения, пока жив слушатель.
pub async fn run(listener: tokio::net::TcpListener, app: Router, header_timeout: Duration) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Кончились дескрипторы и подобное — переждать, а не крутить
                // цикл впустую и не ронять слушателя.
                tracing::warn!(error = %e, "прямая передача: соединение не принято");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let app = app.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
                // `/status` отвечает только самой машине — ему нужен адрес.
                req.extensions_mut().insert(ConnectInfo::<SocketAddr>(peer));
                let app = app.clone();
                async move {
                    Ok::<_, Infallible>(
                        tower::ServiceExt::oneshot(app, req.map(axum::body::Body::new))
                            .await
                            .unwrap_or_else(|e| match e {}),
                    )
                }
            });
            let served = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            if let Err(e) = served {
                tracing::debug!(%peer, error = %e, "прямая передача: соединение закрыто");
            }
        });
    }
}

#[cfg(test)]
#[path = "listen_tests.rs"]
mod tests;
