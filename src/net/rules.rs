// Файл превышает 150 строк: набор правил фаервола целиком — порядок строк и есть смысл, и порознь его не проверить.
//! Правила фаервола для сетей серверов — чистые функции, без вызова iptables.
//!
//! Каждый сервер живёт в своей сети докера, мост которой называется `nr-…`.
//! Поэтому правила общие для всех серверов разом и не меняются, когда сервер
//! заводят или удаляют: шаблон `nr-+` ловит любой мост. Поимённо пишутся
//! только пары внутри групп — прокси и бэкенды за ним.
//!
//! Две цепочки:
//! - `NORO-FWD` из `DOCKER-USER` — транзит: метаданные облака, частные сети,
//!   соседние серверы. `DOCKER-USER` докер проходит первой и никогда не
//!   чистит, поэтому правила переживают его перезапуск.
//! - `NORO-IN` из `INPUT` — сама машина: sshd, Postgres, демон, всё, что
//!   слушает на её адресах. До `DOCKER-USER` такие пакеты не доходят вовсе.
//!
//! Частные сети проверяются по **исходному** адресу назначения (conntrack):
//! после DNAT опубликованного порта адрес уже контейнерный, и иначе мастер на
//! этой же машине, открытый всему интернету, стал бы недоступен её серверам.

use std::net::Ipv4Addr;
use uuid::Uuid;

use super::subnet::Cidr;

pub const FWD_CHAIN: &str = "NORO-FWD";
pub const IN_CHAIN: &str = "NORO-IN";

/// Начало имени моста сети сервера; `+` в iptables — «любое продолжение».
const BRIDGE_PREFIX: &str = "nr-";
pub const ANY_BRIDGE: &str = "nr-+";

/// Мост докера по умолчанию: на нём живут контейнеры, созданные до изоляции,
/// пока их не пересоберут.
const LEGACY_BRIDGE: &str = "docker0";

/// Куда серверу нельзя. Интернет остаётся: моды, авторизация Mojang, мастер
/// по его публичному адресу.
pub const BLOCKED: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

const REJECT: &str = "-j REJECT --reject-with icmp-port-unreachable";

/// Имя моста сети сервера. Ядро режет имена интерфейсов на пятнадцати
/// символах, поэтому от идентификатора берётся начало.
pub fn bridge_name(server: Uuid) -> String {
    format!("{BRIDGE_PREFIX}{}", &server.simple().to_string()[..12])
}

pub fn network_name(server: Uuid) -> String {
    format!("noro-net-{server}")
}

/// Исключение из запретов, заданное оператором ноды.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
    pub net: Cidr,
    /// Без порта — любой порт и протокол.
    pub port: Option<u16>,
}

/// `10.0.0.5`, `10.0.0.0/24`, `10.0.0.5:3306` или `10.0.0.0/24:5432`.
pub fn parse_allow(s: &str) -> Result<Allow, String> {
    let s = s.trim();
    let (net, port) = match s.rsplit_once(':') {
        Some((net, port)) => (
            net,
            Some(
                port.parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or_else(|| format!("порт в «{s}» не число от 1 до 65535"))?,
            ),
        ),
        None => (s, None),
    };
    Ok(Allow {
        net: Cidr::parse(net)?,
        port,
    })
}

/// Хост и порт из адреса мастера: `https://api.example.com` → 443.
pub fn endpoint(url: &str) -> Option<(String, u16)> {
    let url = reqwest::Url::parse(url).ok()?;
    Some((url.host_str()?.to_string(), url.port_or_known_default()?))
}

/// DNS-серверы из `resolv.conf`, кроме петли.
///
/// Встроенный DNS докера ходит к ним изнутри сети контейнера, так что запреты
/// касаются и его. На OCI резолвер — это 169.254.169.254, тот же адрес, что у
/// метаданных: без исключения сервер остался бы без имён вовсе.
pub fn resolvers(resolv_conf: &str) -> Vec<Ipv4Addr> {
    let mut found: Vec<Ipv4Addr> = resolv_conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| rest.trim().parse::<Ipv4Addr>().ok())
        .filter(|ip| !ip.is_loopback())
        .collect();
    found.dedup();
    found
}

/// Сервер группы и его основной порт — тот, что держит демон, пока сервер спит.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub server: Uuid,
    pub port: Option<u16>,
}

pub struct Plan<'a> {
    pub allow: &'a [Allow],
    pub resolvers: &'a [Ipv4Addr],
    pub groups: &'a [Vec<Member>],
}

/// Вход для `iptables-restore --noflush`: обе цепочки заменяются целиком и
/// разом, без мгновения, когда правил нет.
pub fn restore_script(plan: &Plan<'_>) -> String {
    let mut out = vec![
        "*filter".to_string(),
        format!(":{FWD_CHAIN} - [0:0]"),
        format!(":{IN_CHAIN} - [0:0]"),
    ];
    out.extend(forward_rules(plan));
    out.extend(input_rules(plan));
    out.push("COMMIT".into());
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn forward_rules(plan: &Plan<'_>) -> Vec<String> {
    let fwd = |rule: String| format!("-A {FWD_CHAIN} {rule}");
    let established = "-m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT";
    let dnat = "-m conntrack --ctstate DNAT -j ACCEPT";
    let mut rules = vec![
        // Ответы на уже разрешённое. ACCEPT, а не RETURN: у докера старше 28
        // изоляция сетей стоит раньше его же правила про ответы и роняла их.
        fwd(format!("-i {ANY_BRIDGE} {established}")),
        fwd(format!("-o {ANY_BRIDGE} {established}")),
    ];
    let members = || plan.groups.iter().flatten();
    // Прокси, ещё не пересобранный в свою сеть, стучится к бэкенду с моста по
    // умолчанию — до пересборки всей группы связь не должна рваться.
    for m in members() {
        let b = bridge_name(m.server);
        rules.push(fwd(format!("-i {LEGACY_BRIDGE} -o {b} {dnat}")));
    }
    rules.push(fwd(format!("! -i {ANY_BRIDGE} -j RETURN")));
    for group in plan.groups {
        for from in group {
            for to in group.iter().filter(|to| to.server != from.server) {
                let (a, b) = (bridge_name(from.server), bridge_name(to.server));
                // Только через опубликованный порт: внутренние порты соседа
                // (RCON, отладка) группе не открываются.
                rules.push(fwd(format!("-i {a} -o {b} {dnat}")));
            }
        }
    }
    for m in members() {
        let a = bridge_name(m.server);
        rules.push(fwd(format!("-i {a} -o {LEGACY_BRIDGE} {dnat}")));
    }
    for allow in plan.allow {
        rules.extend(allow_rules(allow, "-j ACCEPT").into_iter().map(fwd));
    }
    rules.extend(dns_rules(plan.resolvers).into_iter().map(fwd));
    rules.push(fwd(format!("-o {ANY_BRIDGE} {REJECT}")));
    for net in BLOCKED {
        rules.push(fwd(format!("-m conntrack --ctorigdst {net} {REJECT}")));
    }
    rules
}

fn input_rules(plan: &Plan<'_>) -> Vec<String> {
    let inp = |rule: String| format!("-A {IN_CHAIN} {rule}");
    let mut rules = vec![inp(
        "-m conntrack --ctstate RELATED,ESTABLISHED -j RETURN".into()
    )];
    // Спящий сервер группы: его порт держит демон на самой машине, и прокси
    // должен достучаться до держателя, чтобы разбудить сервер.
    for group in plan.groups {
        for from in group {
            for to in group.iter().filter(|to| to.server != from.server) {
                if let Some(port) = to.port {
                    let a = bridge_name(from.server);
                    rules.push(inp(format!("-i {a} -p tcp --dport {port} -j RETURN")));
                }
            }
        }
    }
    for allow in plan.allow {
        rules.extend(allow_rules(allow, "-j RETURN").into_iter().map(inp));
    }
    rules.extend(dns_rules(plan.resolvers).into_iter().map(inp));
    rules.push(inp(REJECT.into()));
    rules
}

fn allow_rules(allow: &Allow, verdict: &str) -> Vec<String> {
    let dst = format!("-m conntrack --ctorigdst {}", allow.net);
    match allow.port {
        None => vec![format!("{dst} {verdict}")],
        Some(port) => ["tcp", "udp"]
            .iter()
            .map(|proto| format!("-p {proto} {dst} --ctorigdstport {port} {verdict}"))
            .collect(),
    }
}

fn dns_rules(resolvers: &[Ipv4Addr]) -> Vec<String> {
    resolvers
        .iter()
        .flat_map(|ip| {
            ["udp", "tcp"].map(|proto| format!("-d {ip}/32 -p {proto} --dport 53 -j RETURN"))
        })
        .collect()
}

/// Что сделать с переходом в нашу цепочку, глядя на `iptables -S <цепочка>`.
#[derive(Debug, PartialEq, Eq)]
pub enum Hook {
    /// Стоит первым и в одном экземпляре.
    InPlace,
    /// Удалить столько найденных копий и вставить заново первым правилом.
    Reinsert { stale: usize },
}

/// Переход обязан быть первым правилом цепочки: ufw и прочие при перезагрузке
/// вставляют свои ACCEPT наверх, и наш запрет оказался бы за ними.
pub fn hook_state(listing: &str, chain: &str, rule: &str) -> Hook {
    let wanted = format!("-A {chain} {rule}");
    let rules: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(&format!("-A {chain} ")))
        .collect();
    let copies = rules.iter().filter(|l| **l == wanted).count();
    if copies == 1 && rules.first() == Some(&wanted.as_str()) {
        Hook::InPlace
    } else {
        Hook::Reinsert { stale: copies }
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
