//! Кого пускать по SFTP — решает мастер.
//!
//! Нода не хранит ни ключей, ни паролей и не может их перечислить: она
//! спрашивает мастер и получает «да» вместе с корнем и правами. Поэтому
//! скомпрометированная нода не выдаёт чужие ключи и не открывает сессию к
//! серверу, которого не держит.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::link::master::MasterClient;

#[derive(Serialize)]
struct AuthRequest<'a> {
    /// Логин целиком, как его набрал клиент: `игрок.адрес-сервера`.
    username: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<&'a str>,
}

/// Что мастер разрешил этой сессии.
#[derive(Debug, Clone, Deserialize)]
pub struct SftpGrant {
    pub user_id: Uuid,
    pub panel_server_id: Uuid,
    pub can_write: bool,
    pub can_delete: bool,
}

pub async fn authorize(
    master: &MasterClient,
    username: &str,
    public_key: Option<&str>,
    password: Option<&str>,
) -> Result<SftpGrant> {
    let response = master
        .post("/api/node/sftp/auth")
        .json(&AuthRequest {
            username,
            public_key,
            password,
        })
        .send()
        .await?;

    if !response.status().is_success() {
        bail!("мастер отказал: {}", response.status());
    }
    Ok(response.json().await?)
}
