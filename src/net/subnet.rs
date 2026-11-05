//! IPv4-подсети: разбор, пересечение и выбор свободной под сеть сервера.
//!
//! Подсеть каждой сети задаём сами, а не ждём от докера: его пулы по
//! умолчанию — это три десятка сетей на машину, и сорок первый сервер ноды
//! получал бы отказ «all predefined address pools have been fully subnetted».

use std::fmt;
use std::net::Ipv4Addr;

/// Размер сети одного сервера: шлюз, сервер и место под установщик.
pub const SERVER_PREFIX: u8 = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (
                a,
                p.parse::<u8>()
                    .map_err(|_| format!("длина префикса в «{s}» не число"))?,
            ),
            None => (s, 32),
        };
        if prefix > 32 {
            return Err(format!("префикс в «{s}» длиннее 32"));
        }
        let addr: Ipv4Addr = addr.parse().map_err(|_| format!("«{s}» не IPv4-адрес"))?;
        Ok(Cidr {
            addr: Ipv4Addr::from(u32::from(addr) & mask(prefix)),
            prefix,
        })
    }

    fn start(&self) -> u32 {
        u32::from(self.addr)
    }

    fn end(&self) -> u32 {
        self.start() | !mask(self.prefix)
    }

    pub fn overlaps(&self, other: &Cidr) -> bool {
        self.start() <= other.end() && other.start() <= self.end()
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn mask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        p => u32::MAX << (32 - u32::from(p)),
    }
}

/// Первая подсеть длины `prefix` из `pool`, не задевающая ни одну занятую.
///
/// Занятые — подсети всех сетей докера, не только наших: докер откажет в
/// пересечении с любой из них, и узнавать об этом ошибкой незачем.
pub fn pick(pool: Cidr, prefix: u8, used: &[Cidr]) -> Option<Cidr> {
    if prefix < pool.prefix || prefix > 32 {
        return None;
    }
    let step = 1u64 << (32 - u32::from(prefix));
    let count = 1u64 << u32::from(prefix - pool.prefix);
    (0..count)
        .map(|i| Cidr {
            addr: Ipv4Addr::from((u64::from(pool.start()) + i * step) as u32),
            prefix,
        })
        .find(|candidate| !used.iter().any(|u| u.overlaps(candidate)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Cidr {
        Cidr::parse(s).unwrap()
    }

    #[test]
    fn parsing_normalises_the_host_bits() {
        assert_eq!(c("10.209.3.7/16").to_string(), "10.209.0.0/16");
        assert_eq!(c("192.168.1.5").to_string(), "192.168.1.5/32");
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("fd00::/8").is_err());
        assert!(Cidr::parse("10.0.0.0/x").is_err());
    }

    #[test]
    fn overlap_is_symmetric_and_exact_at_the_edges() {
        assert!(c("10.0.0.0/8").overlaps(&c("10.1.2.0/24")));
        assert!(c("10.1.2.0/24").overlaps(&c("10.0.0.0/8")));
        assert!(!c("10.209.0.0/28").overlaps(&c("10.209.0.16/28")));
        assert!(c("10.209.0.15/32").overlaps(&c("10.209.0.0/28")));
    }

    #[test]
    fn the_first_free_subnet_is_taken() {
        let pool = c("10.209.0.0/16");
        assert_eq!(pick(pool, 28, &[]).unwrap().to_string(), "10.209.0.0/28");
        let used = [c("10.209.0.0/28"), c("10.209.0.32/28")];
        assert_eq!(pick(pool, 28, &used).unwrap().to_string(), "10.209.0.16/28");
    }

    /// Чужая сеть докера посреди пула (compose поднял её там же) не
    /// делится — её место просто обходится.
    #[test]
    fn a_foreign_network_inside_the_pool_is_skipped_whole() {
        let pool = c("10.209.0.0/24");
        let used = [c("10.209.0.0/25")];
        assert_eq!(
            pick(pool, 28, &used).unwrap().to_string(),
            "10.209.0.128/28"
        );
    }

    #[test]
    fn an_exhausted_pool_gives_nothing() {
        let pool = c("10.209.0.0/27");
        let used = [c("10.209.0.0/28"), c("10.209.0.16/28")];
        assert_eq!(pick(pool, 28, &used), None);
        assert_eq!(pick(pool, 24, &[]), None, "сеть шире пула не выдаётся");
    }
}
