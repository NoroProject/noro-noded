//! Своя сеть докера у каждого сервера.
//!
//! Мост с предсказуемым именем `nr-…` — то, за что цепляются правила
//! фаервола: они пишутся один раз на шаблон моста, а не на каждый сервер.

use anyhow::{Context, Result};
use bollard::models::{Ipam, IpamConfig, NetworkCreateRequest};
use std::collections::HashMap;

use super::engine::{Engine, LABEL_MANAGED, LABEL_SERVER};
use crate::net::rules::{bridge_name, network_name};
use crate::net::subnet::{self, Cidr};

impl Engine {
    /// Сеть сервера: найденная или созданная. Возвращает её имя для
    /// `NetworkMode` контейнера.
    ///
    /// Вызывающий держит замок: две сети, созданные разом, выбрали бы одну и ту
    /// же свободную подсеть.
    pub async fn ensure_network(&self, server: uuid::Uuid, pool: Cidr) -> Result<String> {
        let name = network_name(server);
        if self.docker.inspect_network(&name, None).await.is_ok() {
            return Ok(name);
        }
        let used = self.used_subnets().await?;
        let subnet = subnet::pick(pool, subnet::SERVER_PREFIX, &used)
            .with_context(|| format!("в пуле {pool} не осталось свободных подсетей"))?;

        let labels = HashMap::from([
            (LABEL_SERVER.to_string(), server.to_string()),
            (LABEL_MANAGED.to_string(), "true".to_string()),
        ]);
        let options = HashMap::from([(
            "com.docker.network.bridge.name".to_string(),
            bridge_name(server),
        )]);
        self.docker
            .create_network(NetworkCreateRequest {
                name: name.clone(),
                driver: Some("bridge".into()),
                ipam: Some(Ipam {
                    config: Some(vec![IpamConfig {
                        subnet: Some(subnet.to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                options: Some(options),
                labels: Some(labels),
                ..Default::default()
            })
            .await
            .with_context(|| format!("не создать сеть {name} ({subnet})"))?;
        Ok(name)
    }

    /// Снять сеть сервера. Её отсутствие — не ошибка: сервер мог жить на мосту
    /// по умолчанию, заведённый до изоляции.
    pub async fn remove_network(&self, server: uuid::Uuid) -> Result<()> {
        let name = network_name(server);
        if self.docker.inspect_network(&name, None).await.is_err() {
            return Ok(());
        }
        self.docker.remove_network(&name).await?;
        Ok(())
    }

    /// Подсети всех сетей докера, IPv4.
    async fn used_subnets(&self) -> Result<Vec<Cidr>> {
        let networks = self.docker.list_networks(None).await?;
        Ok(networks
            .into_iter()
            .filter_map(|n| n.ipam?.config)
            .flatten()
            .filter_map(|c| Cidr::parse(c.subnet.as_deref()?).ok())
            .collect())
    }
}
