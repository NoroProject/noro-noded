//! Установка правил из `rules` в настоящий фаервол.
//!
//! iptables вызывается командой, а не через netlink: тем же бинарником правит
//! и докер, и так правила попадают ровно в тот бэкенд (legacy или nft),
//! которым пользуется он. Вызовы — за трейтом, чтобы порядок действий
//! проверялся без root.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

use super::rules::{self, Hook, ANY_BRIDGE, FWD_CHAIN, IN_CHAIN};

pub trait Iptables: Send + Sync {
    /// `iptables <args>`. `Ok(stdout)` — код выхода ноль.
    fn run(&self, args: &[&str]) -> Result<String>;
    /// `iptables-restore --noflush` с этим входом.
    fn restore(&self, script: &str) -> Result<()>;
}

pub struct SystemIptables;

impl Iptables for SystemIptables {
    fn run(&self, args: &[&str]) -> Result<String> {
        // `-w`: докер правит те же таблицы, и без ожидания замка вызов
        // изредка падал бы на «Another app is currently holding the xtables lock».
        let out = Command::new("iptables")
            .arg("-w")
            .args(args)
            .output()
            .context("iptables не запустился")?;
        if !out.status.success() {
            bail!(
                "iptables {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn restore(&self, script: &str) -> Result<()> {
        let mut child = Command::new("iptables-restore")
            .args(["-w", "--noflush"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("iptables-restore не запустился")?;
        child
            .stdin
            .take()
            .context("нет stdin у iptables-restore")?
            .write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "iptables-restore: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

const DOCKER_USER: &str = "DOCKER-USER";

fn fwd_hook() -> String {
    format!("-j {FWD_CHAIN}")
}

fn in_hook() -> String {
    format!("-i {ANY_BRIDGE} -j {IN_CHAIN}")
}

/// Заменить цепочки и поставить переходы в них первыми правилами.
///
/// Без `DOCKER-USER` (докер с бэкендом nftables, или он ещё не поднялся)
/// переход ставится прямо в `FORWARD`. Когда `DOCKER-USER` появится, следующий
/// проход перенесёт его туда: в `FORWARD` докер вставляет свои ACCEPT выше.
pub fn apply(ipt: &dyn Iptables, script: &str) -> Result<()> {
    ipt.restore(script)?;
    if ipt.run(&["-S", DOCKER_USER]).is_ok() {
        ensure_first(ipt, DOCKER_USER, &fwd_hook())?;
        remove_all(ipt, "FORWARD", &fwd_hook());
    } else {
        ensure_first(ipt, "FORWARD", &fwd_hook())?;
    }
    ensure_first(ipt, "INPUT", &in_hook())
}

/// Снять всё своё: изоляцию выключили в конфиге.
pub fn teardown(ipt: &dyn Iptables) {
    remove_all(ipt, DOCKER_USER, &fwd_hook());
    remove_all(ipt, "FORWARD", &fwd_hook());
    remove_all(ipt, "INPUT", &in_hook());
    for chain in [FWD_CHAIN, IN_CHAIN] {
        let _ = ipt.run(&["-F", chain]);
        let _ = ipt.run(&["-X", chain]);
    }
}

fn ensure_first(ipt: &dyn Iptables, chain: &str, rule: &str) -> Result<()> {
    let listing = ipt.run(&["-S", chain])?;
    let Hook::Reinsert { stale } = rules::hook_state(&listing, chain, rule) else {
        return Ok(());
    };
    let args: Vec<&str> = rule.split(' ').collect();
    for _ in 0..stale {
        ipt.run(&[&["-D", chain][..], &args].concat())?;
    }
    ipt.run(&[&["-I", chain, "1"][..], &args].concat())?;
    Ok(())
}

fn remove_all(ipt: &dyn Iptables, chain: &str, rule: &str) {
    let args: Vec<&str> = rule.split(' ').collect();
    let check = [&["-C", chain][..], &args].concat();
    let delete = [&["-D", chain][..], &args].concat();
    // Предел — на случай фаервола, который отвечает «есть» и не удаляет.
    for _ in 0..16 {
        if ipt.run(&check).is_err() || ipt.run(&delete).is_err() {
            break;
        }
    }
}

#[cfg(test)]
#[path = "firewall_tests.rs"]
mod tests;
