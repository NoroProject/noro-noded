//! Вход по SFTP и пределы сессии.

use super::*;
use crate::sftp::auth::split_login;

#[test]
fn a_login_names_the_player_and_the_server() {
    assert_eq!(split_login("alice.survival"), Some(("alice", "survival")));
    // Ник с точкой тоже бывает: сервер — то, что после **последней** точки.
    assert_eq!(split_login("a.b.test-town"), Some(("a.b", "test-town")));
    assert_eq!(split_login("alice"), None);
    assert_eq!(split_login(".survival"), None);
    assert_eq!(split_login("alice."), None);
}

/// Уникальный идентификатор сессии не участвует в путях — проверка, что
/// корень действительно приходит снаружи, а не собирается из логина.
#[test]
fn the_session_root_is_whatever_the_master_said() {
    let grant = SftpGrant {
        user_id: uuid::Uuid::nil(),
        panel_server_id: uuid::Uuid::nil(),
        can_write: false,
        can_delete: false,
    };
    let (tx, _rx) = mpsc::channel(1);
    let session = SftpSession::new(
        PathBuf::from("/srv/x"),
        grant,
        tx,
        crate::fs::quota::Quota::default(),
    );
    assert_eq!(session.root, PathBuf::from("/srv/x"));
    assert!(session.require_write().is_err(), "без права записи — отказ");
    assert!(session.require_delete().is_err());
}

#[test]
fn open_handles_are_capped() {
    let grant = SftpGrant {
        user_id: uuid::Uuid::nil(),
        panel_server_id: uuid::Uuid::nil(),
        can_write: false,
        can_delete: false,
    };
    let (tx, _rx) = mpsc::channel(1);
    let mut session = SftpSession::new(
        PathBuf::from("/srv/x"),
        grant,
        tx,
        crate::fs::quota::Quota::default(),
    );
    for n in 0..MAX_HANDLES {
        session.dirs.insert(format!("/d{n}"), Vec::new());
    }
    assert!(session.room_for_handle("/another").is_err());
    assert!(
        session.room_for_handle("/d0").is_ok(),
        "повторное открытие той же ручки не занимает места"
    );
}
