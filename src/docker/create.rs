// Файл превышает 150 строк: сборка конфигурации контейнера — одно место, где сходятся лимиты, порты и маунт.
//! Создание контейнера под сервер.
//!
//! Всё, что сюда попадает, приходит из API — поэтому список полей закрыт
//! намеренно. Ни `Binds` сверх одного маунта, ни `Privileged`, ни `CapAdd`, ни
//! `NetworkMode: host` здесь не выставляются и выставлены быть не могут:
//! сокет докера — это root на машине, и поле в форме не должно до него дотянуться.

use anyhow::{bail, Result};
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::CreateContainerOptionsBuilder;
use schema::noded::{PortProtocol, ServerSpec};
use std::collections::HashMap;
use std::path::Path;

use super::engine::{
    Engine, CONTAINER_GID, CONTAINER_ROOT, CONTAINER_UID, LABEL_MANAGED, LABEL_SERVER,
};

pub struct CreateArgs<'a> {
    pub server: uuid::Uuid,
    pub spec: &'a ServerSpec,
    /// Каталог сервера **на хосте** — именно его резолвит докер.
    pub host_dir: &'a Path,
}

impl Engine {
    pub async fn create_container(&self, args: CreateArgs<'_>) -> Result<String> {
        if !super::engine::image_allowed(&args.spec.image) {
            bail!("образ {} не из разрешённого списка", args.spec.image);
        }

        let name = Engine::container_name(args.server);
        let options = CreateContainerOptionsBuilder::default().name(&name).build();

        let mut labels = HashMap::new();
        labels.insert(LABEL_SERVER.to_string(), args.server.to_string());
        labels.insert(LABEL_MANAGED.to_string(), "true".to_string());
        let primary_port = args
            .spec
            .ports
            .iter()
            .find(|p| p.primary)
            .map(|p| p.port)
            .or_else(|| args.spec.ports.first().map(|p| p.port));
        if let Some(port) = primary_port {
            labels.insert(
                super::engine::LABEL_PRIMARY_PORT.to_string(),
                port.to_string(),
            );
        }

        let body = ContainerCreateBody {
            image: Some(args.spec.image.clone()),
            user: Some(format!("{CONTAINER_UID}:{CONTAINER_GID}")),
            working_dir: Some(CONTAINER_ROOT.to_string()),
            cmd: Some(command_line(args.spec)),
            env: Some(env_list(args.spec)),
            labels: Some(labels),
            // Консоль сервера — это stdin: команды уходят туда же, куда их
            // набрал бы человек за машиной.
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(true),
            // tty выключен намеренно: с ним докер склеивает stdout и stderr в
            // один поток без разметки, и отличить строку лога от вывода
            // команды становится нельзя.
            tty: Some(false),
            stdin_once: Some(false),
            exposed_ports: Some(exposed_ports(args.spec)),
            host_config: Some(host_config(args)),
            ..Default::default()
        };

        let created = self.docker.create_container(Some(options), body).await?;
        Ok(created.id)
    }
}

/// Команда запуска: java + аргументы JVM + authlib + jar + аргументы сервера.
///
/// `@libraries/…/unix_args.txt` для NeoForge передаётся как есть — ровно так же
/// это делает Java-враппер, и ломать совместимость со сборками незачем.
///
/// `-javaagent` идёт после пользовательских аргументов и до jar — в том же
/// месте, что у враппера. Без него сервер с `online-mode=true` спрашивает
/// аккаунты у Mojang и не пускает на себя вообще никого: игроки заходят
/// лаунчером, а его учётки живут в нашем Yggdrasil.
fn command_line(spec: &ServerSpec) -> Vec<String> {
    let mut cmd = vec!["java".to_string()];
    cmd.extend(spec.jvm_args.clone());
    if let Some(url) = &spec.authlib_url {
        cmd.push(format!(
            "-javaagent:{}={url}",
            crate::server::layout::Layout::container_authlib_jar()
        ));
    }
    if spec.jar.starts_with('@') {
        cmd.push(spec.jar.clone());
    } else {
        cmd.push("-jar".into());
        cmd.push(spec.jar.clone());
    }
    cmd.extend(spec.server_args.clone());
    cmd
}

fn env_list(spec: &ServerSpec) -> Vec<String> {
    let mut env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    // Стабильный порядок: без него каждый пересбор контейнера выглядит как
    // изменение конфигурации.
    env.sort();
    env
}

fn exposed_ports(spec: &ServerSpec) -> Vec<String> {
    let mut ports = Vec::new();
    for p in &spec.ports {
        for proto in protocols(p.protocol) {
            ports.push(format!("{}/{proto}", p.port));
        }
    }
    ports
}

fn protocols(proto: PortProtocol) -> &'static [&'static str] {
    match proto {
        PortProtocol::Tcp => &["tcp"],
        PortProtocol::Udp => &["udp"],
        PortProtocol::Both => &["tcp", "udp"],
    }
}

/// Общий предел «память плюс своп», как его понимает докер.
///
/// В панели своп задаётся так, как принято у людей: сколько его **сверх**
/// памяти. Докеру нужна сумма, и своп, переданный ему напрямую, он не
/// принимает, когда тот меньше памяти: «Minimum memoryswap limit should be
/// larger than memory limit», и контейнер не создаётся вовсе. Сервер с 15 ГБ
/// памяти и 4 ГБ свопа из-за этого не запускался ни разу.
///
/// Ноль — свопа нет: у докера это и выражается равенством пределов.
/// Отрицательное — без предела.
fn memory_swap(spec: &ServerSpec) -> i64 {
    let memory = spec.memory_mb * 1024 * 1024;
    match spec.swap_mb {
        Some(swap) if swap < 0 => -1,
        Some(swap) => memory + swap * 1024 * 1024,
        None => memory,
    }
}

fn host_config(args: CreateArgs<'_>) -> HostConfig {
    let spec = args.spec;

    let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for p in &spec.ports {
        for proto in protocols(p.protocol) {
            bindings.insert(
                format!("{}/{proto}", p.port),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".into()),
                    host_port: Some(p.port.to_string()),
                }]),
            );
        }
    }

    HostConfig {
        // Ровно один маунт. Больше контейнеру знать о машине не положено.
        binds: Some(vec![format!(
            "{}:{CONTAINER_ROOT}",
            args.host_dir.display()
        )]),
        port_bindings: Some(bindings),
        memory: Some(spec.memory_mb * 1024 * 1024),
        memory_swap: Some(memory_swap(spec)),
        nano_cpus: (spec.cpu_percent > 0).then(|| spec.cpu_percent * 10_000_000),
        pids_limit: (spec.pids_limit > 0).then_some(spec.pids_limit),
        // Убийство по памяти должно оставаться убийством: с выключенным
        // oom-killer контейнер вместо падения намертво зависает в свопе.
        oom_kill_disable: Some(false),
        // Поднимать упавший сервер — дело мастера: он знает про crash loop,
        // приостановку и расписание, а докер перезапускал бы вечно и молча.
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::NO),
            maximum_retry_count: None,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "create_tests.rs"]
mod tests;
