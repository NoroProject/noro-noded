// Файл превышает 150 строк: приём и отдача файла — две половины одного обмена, и порознь их не прочитать.
//! Прямая передача файлов мимо мастера.
//!
//! Мир на двадцать гигабайт незачем гнать через мастер: он упрётся в его канал
//! и в потолок на тело запроса. Если у ноды есть публичный адрес, панель
//! выписывает тикет и браузер ходит к ней напрямую; если адреса нет — байты
//! всё-таки идут через мастер, и это по-прежнему работает, просто медленнее.
//!
//! Тикет и есть вся авторизация этого канала: проверять права здесь нечем — ни
//! сессий, ни токенов ноды у браузера нет. Поэтому тикет одноразовый, живёт
//! минуты и указывает на один конкретный путь, разрешённый заранее.

pub mod tickets;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use schema::noded::TicketMode;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::Daemon;

pub use tickets::Tickets;

pub fn router(daemon: Daemon) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/transfer/{ticket}", get(download).post(upload).put(upload))
        .with_state(daemon)
}

/// Поднять слушателя. Ошибка привязки не валит демон: без прямой передачи он
/// работает, файлы просто идут через мастер.
pub async fn serve(daemon: Daemon) {
    let bind = daemon.cfg.http.bind.clone();
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%bind, error = %e, "прямая передача выключена: не занять порт");
            return;
        }
    };
    tracing::info!(%bind, "прямая передача слушает");

    if let Err(e) = axum::serve(listener, router(daemon)).await {
        tracing::error!(error = %e, "слушатель прямой передачи остановился");
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn download(State(daemon): State<Daemon>, Path(token): Path<String>) -> Response {
    let Some(ticket) = daemon.http_tickets.take(&token) else {
        return (StatusCode::NOT_FOUND, "ticket unknown or expired").into_response();
    };
    if !matches!(ticket.mode, TicketMode::Download) {
        return (StatusCode::METHOD_NOT_ALLOWED, "ticket is for upload").into_response();
    }

    let file = match tokio::fs::File::open(&ticket.path).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);

    // Потоком, а не целиком в память: бэкап мира не обязан помещаться в
    // оперативку демона.
    let body = Body::from_stream(ReaderStream::new(file));
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", safe_name(&ticket.filename)),
            ),
        ],
        body,
    )
        .into_response()
}

async fn upload(
    State(daemon): State<Daemon>,
    Path(token): Path<String>,
    request: Request,
) -> Response {
    let Some(ticket) = daemon.http_tickets.take(&token) else {
        return (StatusCode::NOT_FOUND, "ticket unknown or expired").into_response();
    };
    if !matches!(ticket.mode, TicketMode::Upload) {
        return (StatusCode::METHOD_NOT_ALLOWED, "ticket is for download").into_response();
    }

    // Заявленный размер проверяем до первого байта: принять двадцать гигабайт
    // и отказать в конце — это занятый диск и потерянное время.
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let root = daemon.cfg.server_dir(ticket.server);
    if let Err(e) = daemon.quota.check(ticket.server, &root, declared) {
        return (StatusCode::INSUFFICIENT_STORAGE, e.to_string()).into_response();
    }

    if let Some(parent) = ticket.path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    // Пишем во временный файл рядом и переименовываем: оборванная заливка
    // иначе оставляет обрезанный jar, который сервер пытается загрузить.
    let tmp = ticket.path.with_extension("noro-upload");
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut stream = request.into_body().into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
            }
        };
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        written += chunk.len() as u64;

        // Заголовку верить нельзя: длину можно не прислать вовсе или соврать,
        // поэтому предел проверяется и по ходу.
        if written > declared {
            if let Err(e) = daemon.quota.check(ticket.server, &root, written) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return (StatusCode::INSUFFICIENT_STORAGE, e.to_string()).into_response();
            }
        }
    }

    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    drop(file);

    if let Err(e) = tokio::fs::rename(&tmp, &ticket.path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    // Файл лёг от имени демона, а внутри контейнера сервер ходит другим uid.
    if let Err(e) = crate::server::layout::chown_path(&ticket.path) {
        tracing::warn!(server = %ticket.server, error = %e, "залитый файл остался с чужим владельцем");
    }
    daemon.quota.add(ticket.server, written);

    axum::Json(serde_json::json!({ "bytes": written })).into_response()
}

/// Имя для заголовка: кавычки и переводы строк в нём ломают сам заголовок.
fn safe_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect()
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
