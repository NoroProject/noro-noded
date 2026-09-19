//! Одноразовые тикеты прямой передачи.
//!
//! Живут только в памяти: тикет действует минуты, и переживать перезапуск
//! демона ему незачем — панель выпишет новый. Записывать их на диск значило бы
//! хранить пропуска к чужим файлам дольше, чем они нужны.

use dashmap::DashMap;
use rand::Rng;
use schema::noded::TicketMode;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone)]
pub struct Ticket {
    pub server: Uuid,
    /// Абсолютный путь, уже разрешённый внутри каталога сервера.
    pub path: PathBuf,
    pub mode: TicketMode,
    /// Имя, под которым файл уедет в браузер.
    pub filename: String,
    expires: Instant,
}

#[derive(Clone, Default)]
pub struct Tickets {
    inner: Arc<DashMap<String, Ticket>>,
}

impl Tickets {
    /// Выписать тикет. Секрет — 32 байта hex: он уходит в ссылку, и угадывать
    /// его должно быть незачем даже при полном знании имени файла.
    pub fn issue(
        &self,
        server: Uuid,
        path: PathBuf,
        mode: TicketMode,
        filename: String,
        ttl_secs: u64,
    ) -> String {
        // Потолок на срок: тикет — это доступ к файлу без всякой другой
        // проверки, и «на сутки» такой ссылке жить нельзя.
        let ttl = Duration::from_secs(ttl_secs.clamp(30, 3600));
        let token = hex::encode(rand::thread_rng().gen::<[u8; 32]>());

        self.sweep();
        self.inner.insert(
            token.clone(),
            Ticket {
                server,
                path,
                mode,
                filename,
                expires: Instant::now() + ttl,
            },
        );
        token
    }

    /// Забрать тикет. Одноразовый: повторная ссылка не работает, даже если срок
    /// ещё не вышел — иначе утёкший адрес качают сколько угодно раз.
    pub fn take(&self, token: &str) -> Option<Ticket> {
        let (_, ticket) = self.inner.remove(token)?;
        (ticket.expires > Instant::now()).then_some(ticket)
    }

    /// Протухшие удаляются на каждой выдаче: отдельная задача ради карты,
    /// в которой обычно ноль записей, не нужна.
    fn sweep(&self) {
        let now = Instant::now();
        self.inner.retain(|_, t| t.expires > now);
    }
}
