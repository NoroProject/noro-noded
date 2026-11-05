// Файл превышает 150 строк: установка — одна последовательность, и разрезать её значит потерять порядок шагов.
//! Установка сервера: ядро, EULA, свойства, образ, контейнер.
//!
//! Установщик Forge/NeoForge — это произвольный код из интернета, и запускается
//! он **в одноразовом контейнере целевого образа**, а не на хосте. Иначе любая
//! установка сервера была бы выполнением чужого кода от пользователя демона.

use anyhow::{bail, Context, Result};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    WaitContainerOptionsBuilder,
};
use futures_util::StreamExt;
use schema::noded::{InstallResult, InstallSpec};
use std::path::Path;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::docker::engine::{CONTAINER_GID, CONTAINER_ROOT, CONTAINER_UID};
use crate::docker::{create::CreateArgs, Engine};
use crate::link::master::MasterClient;
use crate::server::entry::find_installed_jar;
pub use crate::server::entry::resolve_entry_point;
use crate::server::layout::Layout;
use crate::server::props;

/// Куда демон складывает authlib-injector. Версия прибита к мастеру: он и
/// раздаёт его лаунчеру, и знает, какой считает своим.
const AUTHLIB_PATH: &str = "/api/agent/authlib-injector.jar";

/// Докачать authlib-injector, если его ещё нет. `true` — качали сейчас.
///
/// Отдельно от установки, потому что контейнер пересоздаётся и мимо неё:
/// аккаунты лаунчера включают и на давно стоящем сервере, а `-javaagent` на
/// несуществующий файл java не прощает — процесс не стартует вообще.
pub async fn ensure_authlib(
    master: &MasterClient,
    layout: &Layout,
    spec: &schema::noded::ServerSpec,
) -> Result<bool> {
    if spec.authlib_url.is_none() {
        return Ok(false);
    }
    let jar = layout.authlib_jar();
    let downloaded = if !jar.is_file() {
        let url = format!("{}{AUTHLIB_PATH}", master.base());
        master
            .download(&url, &jar, None, None)
            .await
            .context("не скачать authlib-injector")?;
        true
    } else {
        false
    };
    let _ = sync_user_jvm_args(&layout.root, spec.authlib_url.as_deref());
    Ok(downloaded)
}

/// Синхронизировать `-javaagent` authlib-injector в `user_jvm_args.txt`.
///
/// Forge (1.17+) и NeoForge читают `@user_jvm_args.txt`. Кроме того, сторонние
/// стартеры (ServerStarter и др.) запускают дочерний Java-процесс именно через
/// `@user_jvm_args.txt`, теряя `-javaagent` родительского контейнера.
/// Добавление агента в `user_jvm_args.txt` гарантирует, что и дочерний процесс
/// сервера запустится с нашим Yggdrasil API.
pub fn sync_user_jvm_args(root: &Path, authlib_url: Option<&str>) -> Result<()> {
    let path = root.join("user_jvm_args.txt");
    if !path.is_file() {
        return Ok(());
    }
    let content = crate::fs::nofollow::read_to_string(&path)?;
    let mut lines: Vec<String> = content
        .lines()
        .filter(|line| !line.trim().contains("authlib-injector.jar"))
        .map(|s| s.to_string())
        .collect();

    if let Some(url) = authlib_url {
        lines.push(format!(
            "-javaagent:{}={url}",
            crate::server::layout::Layout::container_authlib_jar()
        ));
    }

    let mut result = lines.join("\n");
    result.push('\n');
    crate::fs::nofollow::write(&path, result.as_bytes())?;
    Ok(())
}

/// Сервер ставили до того, как нода начала помнить файлы пака.
pub const UNTRACKED_WARNING: &str = "у сервера нет списка файлов прошлого пака: \
     его устаревшие моды не удалены — проверьте mods/";

pub struct InstallCtx<'a> {
    pub engine: &'a Engine,
    pub master: &'a MasterClient,
    pub layout: &'a Layout,
    pub host_dir: &'a Path,
    pub server: Uuid,
    /// Сеть сервера: установщик — чужой код, и до метаданных облака ему не
    /// дотянуться так же, как самому серверу.
    pub network: Option<&'a str>,
    /// Куда писать строки установки, чтобы панель показывала их живьём.
    pub log: mpsc::Sender<String>,
}

pub async fn install(ctx: InstallCtx<'_>, spec: &InstallSpec) -> Result<InstallResult> {
    let root = &ctx.layout.root;
    ctx.layout
        .prepare()
        .context("не подготовить каталог сервера")?;
    let _ = ctx.log.send("подготовлен каталог сервера".into()).await;

    // 1. Образ: в нём и сервер работает, и установщик запускается.
    ctx.engine
        .pull_image(&spec.spec.image)
        .await
        .context("не скачать образ")?;
    let _ = ctx
        .log
        .send(format!("образ готов: {}", spec.spec.image))
        .await;

    // 2. Серверный пак — раньше ядра. Пак, который несёт сервер целиком (GTNH:
    //    Forge, его библиотеки и лаунчер lwjgl3ify), в ядре не нуждается, а
    //    установщик старого Forge и не запускается на нужной такому паку Java.
    let packed = match &spec.server_pack {
        Some(pack) => {
            let laid = crate::server::pack::lay_down(ctx.master, ctx.layout, pack).await?;
            let _ = ctx
                .log
                .send(format!(
                    "серверный пак разложен ({:.1} МБ)",
                    laid.bytes as f64 / 1_048_576.0
                ))
                .await;
            if laid.untracked {
                let _ = ctx.log.send(UNTRACKED_WARNING.into()).await;
            }
            find_installed_jar(root).map(|jar| (jar, laid.bytes))
        }
        None => None,
    };

    // 3. Ядро и установщик — если пак не принёс сервер сам.
    //    Ядро без ссылки — сервер целиком в образе (лимбо): ставить нечего,
    //    запускает его точка входа образа.
    let (jar, bytes) = match packed {
        None if spec.core.url.is_empty() => {
            let _ = ctx
                .log
                .send("сервер поставляется образом, ядро не ставится".into())
                .await;
            (String::new(), 0)
        }
        Some((jar, size)) => {
            let _ = ctx
                .log
                .send(format!("сервер пришёл с паком: {jar}, ядро не ставится"))
                .await;
            (jar, size)
        }
        None => install_core(&ctx, spec).await?,
    };
    let jar = if spec.core.url.is_empty() && spec.server_pack.is_none() {
        jar
    } else {
        resolve_entry_point(root, &jar)
    };

    // 4. EULA и свойства. Без eula.txt сервер пишет строку в лог и выходит —
    //    выглядит это как «не запускается».
    props::accept_eula(&root.join("eula.txt"))?;
    props::apply(&root.join("server.properties"), &spec.properties)?;
    if let Some(port) = spec
        .spec
        .ports
        .iter()
        .find(|p| p.primary)
        .map(|p| p.port)
        .or_else(|| spec.spec.ports.first().map(|p| p.port))
    {
        let _ = ctx.layout.write_primary_port(port);
        props::ensure_port(&root.join("server.properties"), port)?;
    }

    // 5. authlib-injector — внутрь маунта, иначе контейнер его не увидит.
    if ensure_authlib(ctx.master, ctx.layout, &spec.spec).await? {
        let _ = ctx.log.send("authlib-injector установлен".into()).await;
    }

    // 6. Игровой агент. Токен уходит окружением контейнера, а не файлом:
    //    так он не остаётся в каталоге, к которому у владельца есть SFTP.
    if let Some(agent) = &spec.spec.agent {
        crate::server::agent::install(ctx.master, root, agent)
            .await
            .context("не поставить агент")?;
        let _ = ctx
            .log
            .send(format!("агент установлен в {}/", agent.dir))
            .await;
    }

    // 7. Контейнер создаётся остановленным: запускать его решает мастер.
    if ctx.engine.exists(ctx.server).await {
        ctx.engine.remove(ctx.server).await.ok();
    }
    let spec_with_jar = {
        let mut s = spec.spec.clone();
        s.jar = jar.clone();
        s
    };
    ctx.engine
        .create_container(CreateArgs {
            server: ctx.server,
            spec: &spec_with_jar,
            host_dir: ctx.host_dir,
            network: ctx.network,
        })
        .await
        .context("не создать контейнер")?;

    crate::server::layout::chown_recursive(root)?;
    let _ = ctx.log.send("установка завершена".into()).await;

    Ok(InstallResult {
        jar,
        bytes,
        server_pack: spec.server_pack.is_some(),
    })
}

/// Скачать ядро и, если это установщик, прогнать его. Ядро качает нода: это
/// публичный артефакт, и гонять его через мастер значит тратить его канал на
/// то, что и так лежит в открытом доступе.
async fn install_core(ctx: &InstallCtx<'_>, spec: &InstallSpec) -> Result<(String, u64)> {
    let core_name = core_file_name(&spec.core.url);
    let bytes = ctx
        .master
        .download(
            &spec.core.url,
            &ctx.layout.root.join(&core_name),
            spec.core.sha1.as_deref(),
            spec.core.sha256.as_deref(),
        )
        .await
        .context("не скачать ядро сервера")?;
    let _ = ctx.log.send(format!("ядро загружено: {core_name}")).await;

    let jar = if spec.core.installer {
        run_installer(ctx, spec, &core_name).await?
    } else {
        core_name
    };
    Ok((jar, bytes))
}

/// Прогнать установщик в одноразовом контейнере и забрать то, что он собрал.
async fn run_installer(ctx: &InstallCtx<'_>, spec: &InstallSpec, core: &str) -> Result<String> {
    let _ = ctx.log.send("запускаю установщик загрузчика".into()).await;

    let mut cmd = vec![
        "java".to_string(),
        "-jar".to_string(),
        core.to_string(),
        "--installServer".to_string(),
    ];
    cmd.extend(spec.core.installer_args.clone());

    let name = format!("noro-install-{}", ctx.server);
    let options = CreateContainerOptionsBuilder::default().name(&name).build();
    let body = ContainerCreateBody {
        image: Some(spec.spec.image.clone()),
        user: Some(format!("{CONTAINER_UID}:{CONTAINER_GID}")),
        working_dir: Some(CONTAINER_ROOT.to_string()),
        cmd: Some(cmd),
        host_config: Some(crate::docker::create::locked_down(HostConfig {
            binds: Some(vec![format!("{}:{CONTAINER_ROOT}", ctx.host_dir.display())]),
            network_mode: ctx.network.map(str::to_string),
            // Установщику нужна сеть — он тянет библиотеки, — но не нужно
            // ничего сверх неё. Потолок процессов — потому что это чужой код:
            // без него форк-бомба в установщике клала бы всю машину.
            memory: Some(2048 * 1024 * 1024),
            pids_limit: Some(512),
            ..Default::default()
        })),
        ..Default::default()
    };

    // Контейнер с этим именем мог остаться от прошлой попытки, оборванной
    // перезапуском демона, — и тогда каждая следующая установка падала на
    // конфликте имён, пока его не снимут руками.
    remove_installer(ctx.engine, &name).await;
    ctx.engine
        .docker
        .create_container(Some(options), body)
        .await?;

    // Установщик тянет библиотеки из интернета и на мёртвом зеркале висит
    // сколько угодно; мастер перестаёт ждать через полчаса, а контейнер и
    // задача оставались бы навсегда. Потолок чуть меньше мастерского.
    let outcome = tokio::time::timeout(INSTALLER_TIMEOUT, follow_installer(ctx, &name)).await;
    remove_installer(ctx.engine, &name).await;
    let code = match outcome {
        Ok(result) => result?,
        Err(_) => bail!(
            "установщик не уложился в {} минут",
            INSTALLER_TIMEOUT.as_secs() / 60
        ),
    };

    if code != 0 {
        bail!("установщик вышел с кодом {code}");
    }

    find_installed_jar(&ctx.layout.root).context("установщик отработал, но запускать нечего")
}

/// Сколько установщику можно работать.
const INSTALLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25 * 60);

/// Запустить установщик, пересказывая его лог, и дождаться кода выхода.
async fn follow_installer(ctx: &InstallCtx<'_>, name: &str) -> Result<i64> {
    ctx.engine.docker.start_container(name, None).await?;

    let mut logs = ctx.engine.docker.logs(
        name,
        Some(
            LogsOptionsBuilder::default()
                .stdout(true)
                .stderr(true)
                .follow(true)
                .build(),
        ),
    );
    while let Some(chunk) = logs.next().await {
        if let Ok(out) = chunk {
            let line = out.to_string();
            let line = line.trim_end();
            if !line.is_empty() {
                let _ = ctx.log.send(line.to_string()).await;
            }
        }
    }

    let mut wait = ctx
        .engine
        .docker
        .wait_container(name, Some(WaitContainerOptionsBuilder::default().build()));
    let mut code = 0i64;
    while let Some(status) = wait.next().await {
        match status {
            Ok(s) => code = s.status_code,
            // Ненулевой код докер отдаёт ошибкой — это не сбой связи, а
            // сообщение о том, что установщик не справился.
            Err(bollard::errors::Error::DockerContainerWaitError { code: c, .. }) => code = c,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(code)
}

async fn remove_installer(engine: &Engine, name: &str) {
    engine
        .docker
        .remove_container(
            name,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .ok();
}

fn core_file_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|n| n.ends_with(".jar"))
        .unwrap_or("server.jar")
        .to_string()
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
