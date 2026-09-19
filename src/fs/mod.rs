//! Работа с каталогом сервера: один разрешатель пути и операции поверх него.

pub mod archive;
pub mod io;
pub mod list;
pub mod quota;
pub mod safe;

pub use safe::{is_hidden, resolve, resolve_existing};

/// Потолок на текст, который отдаётся в браузер целиком.
///
/// Тот же мегабайт, что у Java-враппера: `latest.log` вырастает до сотен
/// мегабайт, и попытка открыть его в редакторе роняет вкладку, а не сервер.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;
