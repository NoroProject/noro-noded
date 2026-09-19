//! Проверки тикетов: главное здесь — что второй раз по ссылке не пройти.

use super::tickets::Tickets;
use schema::noded::TicketMode;
use std::path::PathBuf;

fn issue(t: &Tickets, mode: TicketMode, ttl: u64) -> String {
    t.issue(
        uuid::Uuid::new_v4(),
        PathBuf::from("/tmp/whatever"),
        mode,
        "file.jar".into(),
        ttl,
    )
}

#[test]
fn a_ticket_works_once() {
    let tickets = Tickets::default();
    let token = issue(&tickets, TicketMode::Download, 60);

    assert!(tickets.take(&token).is_some(), "первый проход");
    assert!(
        tickets.take(&token).is_none(),
        "утёкшую ссылку нельзя использовать повторно"
    );
}

#[test]
fn an_unknown_ticket_is_refused() {
    let tickets = Tickets::default();
    assert!(tickets.take("0".repeat(64).as_str()).is_none());
}

/// Срок обрезается с двух сторон: тикет — это доступ к файлу без всякой другой
/// проверки, и жить сутками такая ссылка не должна.
#[test]
fn the_lifetime_is_clamped() {
    let tickets = Tickets::default();

    // Ноль превратился бы в мгновенно протухший тикет — ссылка не успевала бы
    // дойти до браузера.
    let token = issue(&tickets, TicketMode::Upload, 0);
    assert!(
        tickets.take(&token).is_some(),
        "нижняя граница удержала срок"
    );

    let token = issue(&tickets, TicketMode::Upload, 86_400);
    assert!(tickets.take(&token).is_some());
}

#[test]
fn the_mode_travels_with_the_ticket() {
    let tickets = Tickets::default();
    let token = issue(&tickets, TicketMode::Upload, 60);

    let ticket = tickets.take(&token).expect("тикет выдан");
    assert!(matches!(ticket.mode, TicketMode::Upload));
    assert_eq!(ticket.filename, "file.jar");
}

/// Заголовок `Content-Disposition` собирается строкой, и кавычка в имени файла
/// закрывала бы его раньше времени.
#[test]
fn a_quote_cannot_break_out_of_the_header() {
    assert_eq!(super::safe_name("evil\";drop.jar"), "evil;drop.jar");
    assert_eq!(super::safe_name("line\nbreak.jar"), "linebreak.jar");
}
