use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn start(timeout: Duration) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    tokio::spawn(run(listener, app, timeout));
    addr
}

#[tokio::test]
async fn a_whole_request_is_answered() {
    let addr = start(Duration::from_millis(300)).await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    conn.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("ok"));
}

#[tokio::test]
async fn a_client_that_never_finishes_its_headers_is_dropped() {
    let addr = start(Duration::from_millis(300)).await;
    let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), conn.read_to_end(&mut buf)).await;
    assert!(
        closed.is_ok(),
        "соединение с недописанными заголовками не закрыто"
    );
    assert!(
        !String::from_utf8_lossy(&buf).contains("200"),
        "недописанный запрос не должен обслуживаться"
    );
}
