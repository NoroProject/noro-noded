//! Конфигурация контейнера собирается из данных, пришедших по сети, поэтому
//! проверяется то, чего в ней быть **не должно**, не меньше того, что должно.

use super::*;
use schema::noded::PortMapping;
use std::collections::BTreeMap;

fn spec() -> ServerSpec {
    ServerSpec {
        image: "eclipse-temurin:21-jre".into(),
        memory_mb: 4096,
        swap_mb: None,
        disk_mb: 20480,
        cpu_percent: 200,
        pids_limit: 512,
        jvm_args: vec!["-Xmx3584M".into(), "-XX:+UseG1GC".into()],
        jar: "server.jar".into(),
        server_args: vec!["nogui".into()],
        ports: vec![PortMapping {
            port: 25565,
            protocol: PortProtocol::Both,
            primary: true,
        }],
        env: BTreeMap::new(),
        authlib_url: None,
        agent: None,
    }
}

fn args_for(spec: &ServerSpec) -> CreateArgs<'_> {
    CreateArgs {
        server: uuid::Uuid::nil(),
        spec,
        host_dir: Path::new("/var/lib/noro-noded/servers/x"),
    }
}

#[test]
fn a_plain_jar_is_launched_with_dash_jar() {
    let cmd = command_line(&spec());
    assert_eq!(
        cmd,
        vec![
            "java",
            "-Xmx3584M",
            "-XX:+UseG1GC",
            "-jar",
            "server.jar",
            "nogui"
        ]
    );
}

/// Без `-javaagent` сервер спрашивает аккаунты у Mojang, а игроки заходят
/// лаунчером: `online-mode=true` тогда не пускает вообще никого.
#[test]
fn authlib_injector_lands_before_the_jar() {
    let mut s = spec();
    s.authlib_url = Some("https://noro.example/api/yggdrasil".into());
    let cmd = command_line(&s);

    let agent = cmd
        .iter()
        .position(|a| a.starts_with("-javaagent:"))
        .expect("аргумент есть");
    let jar = cmd.iter().position(|a| a == "-jar").expect("jar есть");
    assert!(agent < jar, "javaagent обязан стоять до jar: {cmd:?}");
    assert_eq!(
        cmd[agent],
        "-javaagent:/home/container/.noro/authlib-injector.jar=https://noro.example/api/yggdrasil"
    );
}

/// Ванильная авторизация остаётся ванильной: сервер без привязки к нашим
/// аккаунтам не должен получать чужой агент в строку запуска.
#[test]
fn without_authlib_the_command_stays_clean() {
    let cmd = command_line(&spec());
    assert!(!cmd.iter().any(|a| a.starts_with("-javaagent:")));
}

/// NeoForge запускается файлом аргументов, а не jar'ом. Java-враппер
/// пробрасывает его как есть, и ломать это незачем.
#[test]
fn a_neoforge_args_file_is_passed_through_untouched() {
    let mut s = spec();
    s.jar = "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt".into();
    let cmd = command_line(&s);
    assert!(
        !cmd.contains(&"-jar".to_string()),
        "-jar тут лишний: {cmd:?}"
    );
    assert_eq!(
        cmd.last().map(String::as_str),
        Some("nogui"),
        "аргументы сервера идут после файла"
    );
    assert!(cmd.contains(&s.jar));
}

/// У Forge и NeoForge в корне jar'а нет вовсе: запуск идёт файлом аргументов,
/// который разворачивается на месте. Поэтому `-javaagent` обязан стоять до
/// него — после разворачивания там уже главный класс, и агент уехал бы ему в
/// аргументы вместо JVM.
#[test]
fn authlib_precedes_a_neoforge_args_file_too() {
    let mut s = spec();
    s.jar = "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt".into();
    s.authlib_url = Some("https://noro.example/api/yggdrasil".into());
    let cmd = command_line(&s);

    let agent = cmd
        .iter()
        .position(|a| a.starts_with("-javaagent:"))
        .expect("аргумент есть");
    let args_file = cmd.iter().position(|a| a == &s.jar).expect("файл есть");
    assert!(
        agent < args_file,
        "javaagent обязан стоять до @-файла: {cmd:?}"
    );
    assert!(
        !cmd.contains(&"-jar".to_string()),
        "-jar тут лишний: {cmd:?}"
    );
}

#[test]
fn limits_reach_the_host_config() {
    let s = spec();
    let hc = host_config(args_for(&s));
    assert_eq!(hc.memory, Some(4096 * 1024 * 1024));
    // Своп равен памяти, иначе лимит обходится свопом.
    assert_eq!(hc.memory_swap, Some(4096 * 1024 * 1024));
    assert_eq!(hc.nano_cpus, Some(2_000_000_000), "200% — это два ядра");
    assert_eq!(hc.pids_limit, Some(512));
    assert_eq!(hc.oom_kill_disable, Some(false));
}

#[test]
fn zero_means_unlimited_rather_than_zero() {
    let mut s = spec();
    s.cpu_percent = 0;
    s.pids_limit = 0;
    let hc = host_config(args_for(&s));
    assert_eq!(hc.nano_cpus, None, "0% — это без ограничения, а не 0 ядер");
    assert_eq!(hc.pids_limit, None);
}

/// Единственный маунт — каталог сервера. Всё остальное на машине контейнера
/// не касается.
#[test]
fn exactly_one_bind_and_nothing_dangerous() {
    let s = spec();
    let hc = host_config(args_for(&s));
    let binds = hc.binds.expect("маунт есть");
    assert_eq!(binds.len(), 1);
    assert_eq!(binds[0], "/var/lib/noro-noded/servers/x:/home/container");

    assert!(hc.privileged.is_none(), "privileged не выставляется");
    assert!(hc.cap_add.is_none(), "лишние capability не выдаются");
    assert!(hc.network_mode.is_none(), "сеть хоста не отдаётся");
    assert!(hc.devices.is_none());
    assert!(hc.security_opt.is_none());
}

/// Докер перезапускал бы сервер вечно и молча, не зная ни про crash loop, ни
/// про приостановку.
#[test]
fn docker_does_not_restart_anything_by_itself() {
    let s = spec();
    let hc = host_config(args_for(&s));
    let policy = hc.restart_policy.expect("политика задана");
    assert_eq!(policy.name, Some(RestartPolicyNameEnum::NO));
}

#[test]
fn both_protocols_get_published() {
    let s = spec();
    let ports = exposed_ports(&s);
    assert!(ports.contains(&"25565/tcp".to_string()));
    assert!(ports.contains(&"25565/udp".to_string()));

    let hc = host_config(args_for(&s));
    let bindings = hc.port_bindings.expect("проброс есть");
    let tcp = bindings
        .get("25565/tcp")
        .expect("tcp")
        .clone()
        .expect("есть");
    assert_eq!(tcp[0].host_port.as_deref(), Some("25565"));
}

#[test]
fn environment_is_stable_between_rebuilds() {
    let mut s = spec();
    s.env.insert("TZ".into(), "UTC".into());
    s.env.insert("AAA".into(), "1".into());
    assert_eq!(
        env_list(&s),
        vec!["AAA=1".to_string(), "TZ=UTC".to_string()]
    );
}

/// Своп в панели задаётся «сверх памяти», а докеру нужна сумма. Меньше памяти
/// он не принимает вовсе, и сервер с 15 ГБ памяти и 4 ГБ свопа не создавался.
#[test]
fn swap_is_added_to_memory() {
    let mut spec = spec();
    spec.memory_mb = 15360;
    spec.swap_mb = Some(4096);
    assert_eq!(super::memory_swap(&spec), (15360 + 4096) * 1024 * 1024);

    spec.swap_mb = Some(0);
    assert_eq!(super::memory_swap(&spec), 15360 * 1024 * 1024);

    spec.swap_mb = None;
    assert_eq!(super::memory_swap(&spec), 15360 * 1024 * 1024);

    spec.swap_mb = Some(-1);
    assert_eq!(super::memory_swap(&spec), -1);
}
