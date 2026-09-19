use super::*;

#[test]
fn varints_survive_a_round_trip() {
    for value in [0, 1, 127, 128, 255, 2_097_151, i32::MAX] {
        let mut buf = Vec::new();
        write_varint(&mut buf, value);
        let mut slice = &buf[..];
        assert_eq!(read_varint(&mut slice).unwrap(), value, "value {value}");
        assert!(slice.is_empty(), "value {value} left bytes behind");
    }
}

#[test]
fn a_negative_varint_terminates() {
    // The handshake sends `-1` as the protocol version. With an arithmetic
    // shift the sign bits keep coming back and the loop never ends.
    let mut buf = Vec::new();
    write_varint(&mut buf, PROTOCOL_ANY);

    assert_eq!(buf.len(), 5);
    let mut slice = &buf[..];
    assert_eq!(read_varint(&mut slice).unwrap(), PROTOCOL_ANY);
}

#[test]
fn a_varint_that_never_ends_is_refused() {
    // Six continuation bytes: a server that sends this is broken or hostile,
    // and reading on would be reading forever.
    let bytes = [0x80u8; 6];
    let mut slice = &bytes[..];
    assert!(read_varint(&mut slice).is_err());
}

#[test]
fn the_plain_string_motd_of_an_old_server_is_read() {
    let json = r#"{"description":"A Minecraft Server","players":{"online":3,"max":20}}"#;
    let pong = parse(json, 12);

    assert!(pong.online);
    assert_eq!(pong.motd.as_deref(), Some("A Minecraft Server"));
    assert_eq!(pong.players_online, Some(3));
    assert_eq!(pong.players_max, Some(20));
}

#[test]
fn a_chat_component_motd_is_flattened() {
    let json = r#"{
        "description": {"text":"Noro","extra":[{"text":" · "},{"text":"survival"}]},
        "players": {"online":0,"max":40},
        "version": {"name":"Paper 1.21.1"}
    }"#;
    let pong = parse(json, 5);

    assert_eq!(pong.motd.as_deref(), Some("Noro · survival"));
    assert_eq!(pong.version.as_deref(), Some("Paper 1.21.1"));
    assert_eq!(pong.players_online, Some(0));
}

#[test]
fn an_answer_we_cannot_parse_still_means_the_server_is_up() {
    // What the caller acts on is "is it alive". A modded server with a broken
    // status plugin is alive.
    let pong = parse("not json at all", 7);

    assert!(pong.online);
    assert_eq!(pong.players_online, None);
    assert_eq!(pong.latency_ms, Some(7));
}

#[test]
fn silence_is_not_an_error() {
    let pong = Pong::silent();

    assert!(!pong.online);
    assert_eq!(pong.players_online, None);
}
