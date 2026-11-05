// Файл превышает 150 строк: состояние изоляции (группы, их сохранение, починка правил, сеть сервера) — одна механика.
//! Изоляция серверов ноды: что сейчас стоит в фаерволе и какие группы знает демон.
//!
//! Группы присылает мастер (`NodeOp::NetworkGroups`), а демон держит их копию
//! на диске: после перезапуска машины правила ставятся до первой связи с
//! мастером, и прокси не теряет бэкенды на время переподключения.
//!
//! Правила переставляются заново раз в минуту. `DOCKER-USER` докер не чистит,
//! но перезагрузка машины, `ufw reload` или `iptables -F` руками стирают всё, и
//! ждать с этим до следующего перезапуска демона нельзя.

use anyhow::{Context, Result};
use schema::noded::{NetworkDiag, NetworkGroup};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use super::firewall::{self, Iptables};
use super::rules::{self, Allow, Member, Plan};
use super::subnet::Cidr;
use crate::config::Config;
use crate::docker::Engine;

const GROUPS_FILE: &str = "network-groups.json";

/// Откуда берутся DNS-серверы. Первый — то, что докер берёт вместо
/// `127.0.0.53` systemd-resolved; второй — всё остальное.
const RESOLV_CONFS: &[&str] = &["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"];

pub struct Isolation {
    enabled: bool,
    /// Адрес мастера: до него серверу нужно всегда — агент, Yggdrasil.
    master_url: String,
    pool: Cidr,
    allow: Vec<Allow>,
    groups_file: PathBuf,
    ipt: Arc<dyn Iptables>,
    groups: parking_lot::Mutex<Vec<NetworkGroup>>,
    /// Последняя установка правил: `None` — стоят, `Some` — почему нет.
    failure: parking_lot::Mutex<Option<String>>,
    /// Одна установка правил за раз и одна новая сеть за раз.
    applying: tokio::sync::Mutex<()>,
    creating: tokio::sync::Mutex<()>,
}

impl Isolation {
    pub fn new(cfg: &Config, ipt: Arc<dyn Iptables>) -> Result<Self> {
        let pool = Cidr::parse(&cfg.network.subnet_pool)
            .map_err(anyhow::Error::msg)
            .context("network.subnet_pool")?;
        let allow = cfg
            .network
            .allow
            .iter()
            .map(|a| rules::parse_allow(a))
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::msg)
            .context("network.allow")?;
        let groups_file = cfg.data_dir.join(GROUPS_FILE);
        let groups = std::fs::read(&groups_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            enabled: cfg.network.isolation,
            master_url: cfg.master_url.clone(),
            pool,
            allow,
            groups_file,
            ipt,
            groups: parking_lot::Mutex::new(groups),
            failure: parking_lot::Mutex::new(None),
            applying: tokio::sync::Mutex::new(()),
            creating: tokio::sync::Mutex::new(()),
        })
    }

    /// Сеть для нового контейнера сервера. `None` — изоляция выключена, и
    /// контейнер встаёт на мост по умолчанию, как до неё.
    pub async fn network_for(&self, engine: &Engine, server: Uuid) -> Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }
        // Правила ставить некуда, а без них отдельные сети только разорвали бы
        // прокси и бэкенды: докер сам режет трафик между сетями.
        if let Some(problem) = host_netns_problem() {
            tracing::warn!(%server, problem, "сервер остаётся на мосту по умолчанию");
            return Ok(None);
        }
        let _one = self.creating.lock().await;
        engine.ensure_network(server, self.pool).await.map(Some)
    }

    /// Поставить правила по текущим группам — или снять, если изоляция
    /// выключена: так её выключение в конфиге не оставляет хвостов.
    pub async fn enforce(&self, engine: &Engine) {
        let _one = self.applying.lock().await;
        let ipt = self.ipt.clone();
        if !self.enabled {
            let _ = tokio::task::spawn_blocking(move || firewall::teardown(&*ipt)).await;
            return;
        }
        let result = match host_netns_problem() {
            Some(problem) => Err(anyhow::anyhow!(problem)),
            None => {
                let groups = self.members(engine).await;
                let mut allow = self.allow.clone();
                allow.extend(master_allow(&self.master_url).await);
                let script = rules::restore_script(&Plan {
                    allow: &allow,
                    resolvers: &read_resolvers(),
                    groups: &groups,
                });
                tokio::task::spawn_blocking(move || firewall::apply(&*ipt, &script))
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("установка правил оборвалась: {e}")))
            }
        };
        let failure = result.err().map(|e| format!("{e:#}"));
        let mut last = self.failure.lock();
        if failure != *last {
            match &failure {
                Some(e) => tracing::error!(error = %e, "изоляция сети не установлена"),
                None => tracing::info!("правила изоляции сети на месте"),
            }
        }
        *last = failure;
    }

    /// Новые группы от мастера: сохранить и сразу поставить.
    pub async fn set_groups(&self, engine: &Engine, groups: Vec<NetworkGroup>) -> Result<()> {
        save(&self.groups_file, &groups)?;
        *self.groups.lock() = groups;
        self.enforce(engine).await;
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Сервер удалён: убрать его из групп и снять его сеть.
    pub async fn forget(&self, engine: &Engine, server: Uuid) {
        // Сеть могла остаться с тех пор, как изоляция была включена.
        if let Err(e) = engine.remove_network(server).await {
            tracing::warn!(%server, error = %e, "сеть сервера не снята");
        }
        if !self.enabled {
            return;
        }
        let groups = without(&self.groups.lock(), server);
        if let Err(e) = save(&self.groups_file, &groups) {
            tracing::warn!(%server, error = %e, "группы сети не сохранены");
        }
        *self.groups.lock() = groups;
        self.enforce(engine).await;
    }

    pub fn diag(&self) -> NetworkDiag {
        let failure = self.failure.lock().clone();
        NetworkDiag {
            enabled: self.enabled,
            enforced: self.enabled && failure.is_none(),
            error: failure,
            groups: self.groups.lock().len() as u32,
        }
    }

    /// Порт каждого участника — из метки его контейнера. Не из файла в
    /// каталоге сервера: тот пишет сам сервер, и подставленный туда `22`
    /// открыл бы соседу по группе sshd машины.
    async fn members(&self, engine: &Engine) -> Vec<Vec<Member>> {
        let groups = self.groups.lock().clone();
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut members = Vec::with_capacity(group.servers.len());
            for server in group.servers {
                members.push(Member {
                    server,
                    port: engine.primary_port(server).await,
                });
            }
            out.push(members);
        }
        out
    }
}

/// Мастер — всегда разрешён, на его порт.
///
/// Агент сервера и authlib-injector ходят к нему по публичному адресу. Где
/// этот адрес принадлежит самой машине, а мастер слушает прямо на ней (traefik
/// в сети хоста), запрет служб хоста отрезал бы сервер от входа игроков.
/// Имя разрешается заново при каждой установке правил: адрес мастера может
/// смениться, а держать старый незачем.
async fn master_allow(master_url: &str) -> Vec<Allow> {
    let Some((host, port)) = rules::endpoint(master_url) else {
        return Vec::new();
    };
    let resolved = tokio::net::lookup_host((host.as_str(), port)).await;
    let mut out: Vec<Allow> = Vec::new();
    for addr in resolved.into_iter().flatten() {
        if let std::net::IpAddr::V4(ip) = addr.ip() {
            let allow = Allow {
                net: Cidr {
                    addr: ip,
                    prefix: 32,
                },
                port: Some(port),
            };
            if !out.contains(&allow) {
                out.push(allow);
            }
        }
    }
    out
}

/// Группы без удалённого сервера; группа из одного больше не группа.
fn without(groups: &[NetworkGroup], server: Uuid) -> Vec<NetworkGroup> {
    groups
        .iter()
        .map(|g| NetworkGroup {
            id: g.id,
            servers: g.servers.iter().copied().filter(|s| *s != server).collect(),
        })
        .filter(|g| g.servers.len() > 1)
        .collect()
}

fn save(path: &Path, groups: &[NetworkGroup]) -> Result<()> {
    crate::fs::nofollow::write(path, &serde_json::to_vec(groups)?)
}

fn read_resolvers() -> Vec<Ipv4Addr> {
    let mut all = Vec::new();
    for path in RESOLV_CONFS {
        if let Ok(text) = std::fs::read_to_string(path) {
            for ip in rules::resolvers(&text) {
                if !all.contains(&ip) {
                    all.push(ip);
                }
            }
        }
    }
    all
}

/// Демон в контейнере видит свою сеть, а не сеть машины: правила легли бы
/// туда и не защищали бы ничего, а отчёт говорил бы «на месте».
fn host_netns_problem() -> Option<&'static str> {
    let in_container = Path::new("/.dockerenv").exists();
    let sees_docker_bridge = Path::new("/sys/class/net/docker0").exists();
    (in_container && !sees_docker_bridge)
        .then_some("демон работает в контейнере без сети хоста: правила фаервола ставить некуда")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deleted_server_leaves_its_group_and_a_lone_member_dissolves_it() {
        let (p, a, b) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let groups = vec![
            NetworkGroup {
                id: p,
                servers: vec![p, a, b],
            },
            NetworkGroup {
                id: Uuid::from_u128(9),
                servers: vec![Uuid::from_u128(9), a],
            },
        ];
        let left = without(&groups, a);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].servers, vec![p, b]);
    }
}
