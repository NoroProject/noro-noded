use super::*;

/// Номер — в старших битах: имя моста берётся с начала идентификатора, и у
/// `from_u128(1)` и `from_u128(2)` оно совпало бы.
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n << 96)
}

fn member(n: u128, port: Option<u16>) -> Member {
    Member {
        server: id(n),
        port,
    }
}

fn script(allow: &[Allow], resolvers: &[Ipv4Addr], groups: &[Vec<Member>]) -> String {
    restore_script(&Plan {
        allow,
        resolvers,
        groups,
    })
}

fn position(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("нет строки «{needle}» в\n{text}"))
}

#[test]
fn a_bridge_name_fits_the_kernel_limit_and_is_stable() {
    let server = Uuid::parse_str("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0").unwrap();
    assert_eq!(bridge_name(server), "nr-0f1e2d3c4b5a");
    assert!(bridge_name(server).len() <= 15);
    assert_eq!(
        network_name(server),
        "noro-net-0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
    );
}

#[test]
fn allow_entries_parse_with_and_without_a_port() {
    assert_eq!(
        parse_allow("10.0.0.5:3306").unwrap(),
        Allow {
            net: Cidr::parse("10.0.0.5").unwrap(),
            port: Some(3306)
        }
    );
    assert_eq!(parse_allow(" 10.0.0.0/24 ").unwrap().port, None);
    assert_eq!(parse_allow("10.0.0.0/24:5432").unwrap().port, Some(5432));
    assert!(parse_allow("10.0.0.5:0").is_err());
    assert!(parse_allow("10.0.0.5:http").is_err());
    assert!(parse_allow("db.local").is_err());
}

#[test]
fn the_master_endpoint_takes_the_scheme_port_unless_given() {
    assert_eq!(
        endpoint("https://api.noro.rest"),
        Some(("api.noro.rest".into(), 443))
    );
    assert_eq!(
        endpoint("http://10.0.0.5:8080"),
        Some(("10.0.0.5".into(), 8080))
    );
    assert_eq!(endpoint("not a url"), None);
}

/// OCI кладёт свой резолвер на адрес метаданных; systemd-resolved — на петлю,
/// и та докеру не нужна: к ней он ходит сам, с хоста.
#[test]
fn resolvers_come_from_resolv_conf_without_loopback() {
    let conf = "# generated\nnameserver 169.254.169.254\nnameserver 127.0.0.53\n\
                nameserver fd00::1\nsearch vcn.oraclevcn.com\nnameserver 10.0.0.2\n";
    assert_eq!(
        resolvers(conf),
        vec![
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(10, 0, 0, 2)
        ]
    );
}

#[test]
fn the_script_replaces_both_chains_in_one_transaction() {
    let text = script(&[], &[], &[]);
    assert!(text.starts_with("*filter\n:NORO-FWD - [0:0]\n:NORO-IN - [0:0]\n"));
    assert!(text.ends_with("COMMIT\n"));
}

/// Порядок — это и есть политика: разрешения обязаны стоять выше запретов,
/// а чужой трафик — уходить из цепочки раньше, чем его тронет запрет.
#[test]
fn forward_rules_allow_before_they_refuse() {
    let allow = [parse_allow("10.0.0.5:3306").unwrap()];
    let dns = [Ipv4Addr::new(169, 254, 169, 254)];
    let groups = [vec![member(1, Some(25565)), member(2, Some(25566))]];
    let text = script(&allow, &dns, &groups);
    let (a, b) = (bridge_name(id(1)), bridge_name(id(2)));

    let established = position(&text, "-A NORO-FWD -i nr-+ -m conntrack --ctstate RELATED");
    let foreign = position(&text, "-A NORO-FWD ! -i nr-+ -j RETURN");
    let pair = position(
        &text,
        &format!("-A NORO-FWD -i {a} -o {b} -m conntrack --ctstate DNAT -j ACCEPT"),
    );
    let reverse = position(&text, &format!("-A NORO-FWD -i {b} -o {a} "));
    let allowed = position(
        &text,
        "-A NORO-FWD -p tcp -m conntrack --ctorigdst 10.0.0.5/32 --ctorigdstport 3306 -j ACCEPT",
    );
    let resolver = position(
        &text,
        "-A NORO-FWD -d 169.254.169.254/32 -p udp --dport 53 -j RETURN",
    );
    let neighbours = position(&text, "-A NORO-FWD -o nr-+ -j REJECT");
    let metadata = position(
        &text,
        "-A NORO-FWD -m conntrack --ctorigdst 169.254.0.0/16 -j REJECT",
    );

    assert!(established < foreign);
    assert!(foreign < pair && pair < neighbours && reverse < neighbours);
    assert!(allowed < neighbours && resolver < metadata);
    assert!(neighbours < metadata);
    assert!(text.contains("--ctorigdst 10.0.0.0/8 -j REJECT"));
    assert!(text.contains("--ctorigdst 192.168.0.0/16 -j REJECT"));
}

/// Контейнер, не пересобранный в свою сеть, сидит на docker0. Пока группа
/// пересобирается по одному, прокси и бэкенд не должны терять друг друга.
#[test]
fn legacy_bridge_reaches_group_members_before_foreign_traffic_leaves() {
    let groups = [vec![member(1, None), member(2, None)]];
    let text = script(&[], &[], &groups);
    let a = bridge_name(id(1));
    let legacy_in = position(
        &text,
        &format!("-A NORO-FWD -i docker0 -o {a} -m conntrack --ctstate DNAT -j ACCEPT"),
    );
    let legacy_out = position(&text, &format!("-A NORO-FWD -i {a} -o docker0 "));
    let foreign = position(&text, "-A NORO-FWD ! -i nr-+ -j RETURN");
    let neighbours = position(&text, "-A NORO-FWD -o nr-+ -j REJECT");
    assert!(legacy_in < foreign);
    assert!(legacy_out < neighbours);
}

#[test]
fn every_pair_of_a_group_gets_its_own_rule() {
    let groups = [vec![member(1, None), member(2, None), member(3, None)]];
    let text = script(&[], &[], &groups);
    let pairs: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("-A NORO-FWD -i nr-") && l.contains(" -o nr-"))
        .collect();
    assert_eq!(pairs.len(), 6, "три сервера — шесть направлений");
    let unique: std::collections::HashSet<_> = pairs.iter().collect();
    assert_eq!(unique.len(), 6);
}

#[test]
fn a_server_outside_groups_gets_no_pair_rules() {
    let text = script(&[], &[], &[]);
    assert!(!text.contains("--ctstate DNAT"));
    assert!(!text.contains("--dport 53"));
}

#[test]
fn the_host_is_closed_except_replies_sleeping_peers_and_dns() {
    let dns = [Ipv4Addr::new(10, 0, 0, 2)];
    let groups = [vec![member(1, Some(25565)), member(2, None)]];
    let text = script(&[parse_allow("10.0.0.9").unwrap()], &dns, &groups);
    let b = bridge_name(id(2));

    let replies = position(
        &text,
        "-A NORO-IN -m conntrack --ctstate RELATED,ESTABLISHED -j RETURN",
    );
    // Второй сервер может будить первого; у второго порта нет — и правила тоже.
    let wake = position(
        &text,
        &format!("-A NORO-IN -i {b} -p tcp --dport 25565 -j RETURN"),
    );
    assert_eq!(text.matches("-A NORO-IN -i nr-").count(), 1);
    let allowed = position(
        &text,
        "-A NORO-IN -m conntrack --ctorigdst 10.0.0.9/32 -j RETURN",
    );
    let resolver = position(
        &text,
        "-A NORO-IN -d 10.0.0.2/32 -p tcp --dport 53 -j RETURN",
    );
    let refuse = position(&text, "-A NORO-IN -j REJECT");
    assert!(replies < wake && wake < refuse && allowed < refuse && resolver < refuse);
}

#[test]
fn a_hook_is_in_place_only_as_the_single_first_rule() {
    let rule = "-j NORO-FWD";
    let good = "-N DOCKER-USER\n-A DOCKER-USER -j NORO-FWD\n-A DOCKER-USER -j RETURN\n";
    assert_eq!(hook_state(good, "DOCKER-USER", rule), Hook::InPlace);

    let missing = "-N DOCKER-USER\n-A DOCKER-USER -j RETURN\n";
    assert_eq!(
        hook_state(missing, "DOCKER-USER", rule),
        Hook::Reinsert { stale: 0 }
    );

    let pushed_down =
        "-P INPUT ACCEPT\n-A INPUT -j ufw-before-input\n-A INPUT -i nr-+ -j NORO-IN\n";
    assert_eq!(
        hook_state(pushed_down, "INPUT", "-i nr-+ -j NORO-IN"),
        Hook::Reinsert { stale: 1 }
    );

    let doubled = "-A DOCKER-USER -j NORO-FWD\n-A DOCKER-USER -j NORO-FWD\n";
    assert_eq!(
        hook_state(doubled, "DOCKER-USER", rule),
        Hook::Reinsert { stale: 2 }
    );
}
