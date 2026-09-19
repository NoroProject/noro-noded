use super::*;
use crate::server::ping;
use schema::noded::Listing;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build the handshake a client sends first.
fn handshake(host: &str, port: u16, next_state: i32) -> Vec<u8> {
    let mut body = Vec::new();
    write_varint(&mut body, 0x00);
    write_varint(&mut body, 767); // any protocol version
    write_varint(&mut body, host.len() as i32);
    body.extend_from_slice(host.as_bytes());
    body.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut body, next_state);
    body
}

#[test]
fn the_wanted_state_is_read_out_of_the_handshake() {
    assert_eq!(
        parse_handshake(&handshake("noro.example", 25565, 1)).unwrap(),
        1
    );
    assert_eq!(
        parse_handshake(&handshake("noro.example", 25565, 2)).unwrap(),
        2
    );
}

#[test]
fn a_handshake_claiming_a_host_longer_than_itself_is_refused() {
    // Without the length check this reads past the packet — the classic way a
    // parser is turned into a crash.
    let mut body = Vec::new();
    write_varint(&mut body, 0x00);
    write_varint(&mut body, 767);
    write_varint(&mut body, 200); // host length, with nothing behind it
    body.extend_from_slice(b"short");

    assert!(parse_handshake(&body).is_err());
}

#[test]
fn something_that_is_not_a_handshake_is_refused() {
    let mut body = Vec::new();
    write_varint(&mut body, 0x42);

    assert!(parse_handshake(&body).is_err());
}

#[tokio::test]
async fn a_sleeping_server_answers_the_server_list_with_its_motd() {
    let port = free_port().await;
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = woken.clone();

    let held = hold(
        Uuid::new_v4(),
        port,
        listing("Sleeping · join to start"),
        Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await
    .expect("port held");

    let pong = ping::ping("127.0.0.1", port).await;

    assert!(pong.online, "the placeholder has to answer");
    assert_eq!(pong.motd.as_deref(), Some("Sleeping · join to start"));
    // A status request is not a login: looking at the server list must not
    // start a machine.
    assert_eq!(woken.load(Ordering::SeqCst), 0);

    held.release();
}

#[tokio::test]
async fn a_login_attempt_wakes_the_server_once() {
    let port = free_port().await;
    let server = Uuid::new_v4();
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = woken.clone();

    let held = hold(
        server,
        port,
        listing("Starting"),
        Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await
    .expect("port held");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(&framed(&handshake("noro.example", port, STATE_LOGIN)))
        .await
        .unwrap();

    // The refusal comes back, so the player sees why nothing happened.
    let mut buf = [0u8; 128];
    let read = stream.read(&mut buf).await.unwrap();
    assert!(read > 0, "the placeholder has to say something");

    // Give the spawned task a moment to run the callback.
    for _ in 0..50 {
        if woken.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(woken.load(Ordering::SeqCst), 1);

    held.release();
}

#[tokio::test]
async fn releasing_frees_the_port_for_the_real_server() {
    // The container needs this port itself; two listeners on it is one refusal
    // at the worst possible moment.
    let port = free_port().await;
    let held = hold(Uuid::new_v4(), port, listing("x"), Arc::new(|_| {}))
        .await
        .expect("port held");

    held.release();

    for _ in 0..50 {
        if TcpListener::bind(("0.0.0.0", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("port is still held");
}

fn listing(motd: &str) -> Listing {
    Listing {
        motd: motd.to_string(),
        ..Listing::default()
    }
}

async fn free_port() -> u16 {
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}
