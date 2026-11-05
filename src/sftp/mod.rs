//! SFTP-доступ к файлам сервера.
//!
//! Нужен ровно для того, чего не умеет браузер: залить модпак на сотни
//! мегабайт и вытащить мир. Аутентификацию решает мастер, пути проходят через
//! общий разрешатель, права — те же, что у файлового менеджера.

pub mod auth;
pub mod handler;
pub mod server;

/// Удаление мира или листинг на десятки тысяч файлов шли прямо на рабочем
/// потоке рантайма и останавливали консоли всей ноды. `block_in_place` отдаёт
/// остальные задачи другим потокам, не вынося состояние сессии в задачу.
pub(crate) fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}
