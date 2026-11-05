use super::*;
use anyhow::anyhow;
use std::collections::BTreeMap;

/// Таблица filter в памяти: ровно столько iptables, сколько трогает `apply`.
#[derive(Default)]
pub struct FakeIptables {
    pub chains: parking_lot::Mutex<BTreeMap<String, Vec<String>>>,
}

impl FakeIptables {
    pub fn with(chains: &[(&str, &[&str])]) -> Self {
        let fake = Self::default();
        for (chain, rules) in chains {
            fake.chains.lock().insert(
                chain.to_string(),
                rules.iter().map(|r| r.to_string()).collect(),
            );
        }
        fake
    }

    pub fn rules(&self, chain: &str) -> Vec<String> {
        self.chains.lock().get(chain).cloned().unwrap_or_default()
    }
}

impl Iptables for FakeIptables {
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut chains = self.chains.lock();
        let (cmd, chain) = (args[0], args[1]);
        let rest = |from: usize| args[from..].join(" ");
        let rules = chains
            .get_mut(chain)
            .ok_or_else(|| anyhow!("No chain/target/match by that name"))?;
        match cmd {
            "-S" => Ok(std::iter::once(format!("-N {chain}"))
                .chain(rules.iter().map(|r| format!("-A {chain} {r}")))
                .collect::<Vec<_>>()
                .join("\n")),
            "-C" => rules
                .contains(&rest(2))
                .then(String::new)
                .ok_or_else(|| anyhow!("Bad rule")),
            "-D" => {
                let at = rules
                    .iter()
                    .position(|r| *r == rest(2))
                    .ok_or_else(|| anyhow!("Bad rule"))?;
                rules.remove(at);
                Ok(String::new())
            }
            "-I" => {
                rules.insert(0, rest(3));
                Ok(String::new())
            }
            "-F" => {
                rules.clear();
                Ok(String::new())
            }
            "-X" => {
                chains.remove(chain);
                Ok(String::new())
            }
            other => Err(anyhow!("fake iptables: {other}")),
        }
    }

    fn restore(&self, script: &str) -> Result<()> {
        let mut chains = self.chains.lock();
        for line in script.lines() {
            if let Some(decl) = line.strip_prefix(':') {
                let name = decl.split(' ').next().unwrap().to_string();
                chains.insert(name, Vec::new());
            } else if let Some(rule) = line.strip_prefix("-A ") {
                let (chain, rest) = rule.split_once(' ').unwrap();
                chains
                    .get_mut(chain)
                    .ok_or_else(|| anyhow!("restore: no chain {chain}"))?
                    .push(rest.to_string());
            }
        }
        Ok(())
    }
}

const SCRIPT: &str = "*filter\n:NORO-FWD - [0:0]\n:NORO-IN - [0:0]\n-A NORO-FWD -j RETURN\n-A NORO-IN -j REJECT\nCOMMIT\n";

#[test]
fn hooks_go_first_into_docker_user_and_input() {
    let ipt = FakeIptables::with(&[
        ("DOCKER-USER", &["-j RETURN"]),
        ("FORWARD", &["-j DOCKER-USER", "-j DOCKER-FORWARD"]),
        ("INPUT", &["-j ufw-before-input"]),
    ]);
    apply(&ipt, SCRIPT).unwrap();
    assert_eq!(ipt.rules("DOCKER-USER"), vec!["-j NORO-FWD", "-j RETURN"]);
    assert_eq!(
        ipt.rules("FORWARD"),
        vec!["-j DOCKER-USER", "-j DOCKER-FORWARD"]
    );
    assert_eq!(
        ipt.rules("INPUT"),
        vec!["-i nr-+ -j NORO-IN", "-j ufw-before-input"]
    );
    assert_eq!(ipt.rules("NORO-IN"), vec!["-j REJECT"]);
}

/// Повтор каждую минуту не должен плодить переходы, а сдвинутый вниз переход
/// (ufw перезагрузился) — возвращается наверх.
#[test]
fn applying_again_is_idempotent_and_repairs_the_order() {
    let ipt = FakeIptables::with(&[("DOCKER-USER", &[]), ("FORWARD", &[]), ("INPUT", &[])]);
    apply(&ipt, SCRIPT).unwrap();
    apply(&ipt, SCRIPT).unwrap();
    assert_eq!(ipt.rules("DOCKER-USER"), vec!["-j NORO-FWD"]);

    ipt.chains
        .lock()
        .get_mut("INPUT")
        .unwrap()
        .insert(0, "-j ufw-before-input".into());
    apply(&ipt, SCRIPT).unwrap();
    assert_eq!(
        ipt.rules("INPUT"),
        vec!["-i nr-+ -j NORO-IN", "-j ufw-before-input"]
    );
}

/// Докер с бэкендом nftables цепочки DOCKER-USER не заводит: тогда переход
/// живёт в FORWARD, а появилась она — переезжает туда.
#[test]
fn without_docker_user_the_hook_lives_in_forward_until_it_appears() {
    let ipt = FakeIptables::with(&[("FORWARD", &["-j ACCEPT"]), ("INPUT", &[])]);
    apply(&ipt, SCRIPT).unwrap();
    assert_eq!(ipt.rules("FORWARD"), vec!["-j NORO-FWD", "-j ACCEPT"]);

    ipt.chains.lock().insert("DOCKER-USER".into(), Vec::new());
    apply(&ipt, SCRIPT).unwrap();
    assert_eq!(ipt.rules("FORWARD"), vec!["-j ACCEPT"]);
    assert_eq!(ipt.rules("DOCKER-USER"), vec!["-j NORO-FWD"]);
}

#[test]
fn teardown_leaves_nothing_of_ours() {
    let ipt = FakeIptables::with(&[("DOCKER-USER", &[]), ("FORWARD", &[]), ("INPUT", &[])]);
    apply(&ipt, SCRIPT).unwrap();
    teardown(&ipt);
    assert!(ipt.rules("DOCKER-USER").is_empty());
    assert!(ipt.rules("INPUT").is_empty());
    assert!(!ipt.chains.lock().contains_key("NORO-FWD"));
    assert!(!ipt.chains.lock().contains_key("NORO-IN"));
}

#[test]
fn a_failing_restore_installs_no_hooks() {
    struct Broken;
    impl Iptables for Broken {
        fn run(&self, _: &[&str]) -> Result<String> {
            panic!("после отказа restore вызовов быть не должно");
        }
        fn restore(&self, _: &str) -> Result<()> {
            Err(anyhow!("iptables-restore: not found"))
        }
    }
    assert!(apply(&Broken, SCRIPT).is_err());
}
