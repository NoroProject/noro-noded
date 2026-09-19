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
use crate::server::layout::Layout;
use crate::server::props;

/// Куда демон складывает authlib-injector. Версия прибита к мастеру: он и
/// раздаёт его лаунчеру, и знает, какой считает своим.
const AUTHLIB_PATH: &str = "/api/agent/authlib-injector.jar";

pub struct InstallCtx<'a> {
    pub engine: &'a Engine,
    pub master: &'a MasterClient,
    pub layout: &'a Layout,
    pub host_dir: &'a Path,
    pub server: Uuid,
    /// Куда писать строки установки, чтобы панель показывала их живьём.
    pub log: mpsc::Sender<String>,
}

pub async fn install(ctx: InstallCtx<'_>, spec: &InstallSpec) -> Result<InstallResult> {
    let root = &ctx.layout.root;
    ctx.layout
        .prepare()
        .context("не подготовить каталог сервера")?;
    let _ = ctx.log.send("подготовлен каталог сервера".into()).await;

    // 1. Ядро. Качает нода: это публичный артефакт, и гонять его через мастер
    //    значит тратить его канал на то, что и так лежит в открытом доступе.
    let core_name = core_file_name(&spec.core.url);
    let core_path = root.join(&core_name);
    let bytes = ctx
        .master
        .download(
            &spec.core.url,
            &core_path,
            spec.core.sha1.as_deref(),
            spec.core.sha256.as_deref(),
        )
        .await
        .context("не скачать ядро сервера")?;
    let _ = ctx.log.send(format!("ядро загружено: {core_name}")).await;

    // 2. Образ — до установщика: он в этом же образе и запускается.
    ctx.engine
        .pull_image(&spec.spec.image)
        .await
        .context("не скачать образ")?;
    let _ = ctx
        .log
        .send(format!("образ готов: {}", spec.spec.image))
        .await;

    // 3. Установщик, если ядро им является.
    let jar = if spec.core.installer {
        run_installer(&ctx, spec, &core_name).await?
    } else {
        core_name.clone()
    };

    // 4. EULA и свойства. Без eula.txt сервер пишет строку в лог и выходит —
    //    выглядит это как «не запускается».
    props::accept_eula(&root.join("eula.txt"))?;
    props::apply(&root.join("server.properties"), &spec.properties)?;

    // 5. authlib-injector — внутрь маунта, иначе контейнер его не увидит.
    if spec.spec.authlib_url.is_some() {
        let url = format!("{}{AUTHLIB_PATH}", ctx.master.base());
        ctx.master
            .download(&url, &ctx.layout.authlib_jar(), None, None)
            .await
            .context("не скачать authlib-injector")?;
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
        })
        .await
        .context("не создать контейнер")?;

    crate::server::layout::chown_recursive(root)?;
    let _ = ctx.log.send("установка завершена".into()).await;

    Ok(InstallResult { jar, bytes })
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
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{}:{CONTAINER_ROOT}", ctx.host_dir.display())]),
            // Установщику нужна сеть — он тянет библиотеки, — но не нужно
            // ничего сверх неё.
            memory: Some(2048 * 1024 * 1024),
            ..Default::default()
        }),
        ..Default::default()
    };

    ctx.engine
        .docker
        .create_container(Some(options), body)
        .await?;
    ctx.engine.docker.start_container(&name, None).await?;

    let mut logs = ctx.engine.docker.logs(
        &name,
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
        .wait_container(&name, Some(WaitContainerOptionsBuilder::default().build()));
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

    ctx.engine
        .docker
        .remove_container(
            &name,
            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
        )
        .await
        .ok();

    if code != 0 {
        bail!("установщик вышел с кодом {code}");
    }

    find_installed_jar(&ctx.layout.root).context("установщик отработал, но запускать нечего")
}

/// Что запускать после установщика.
///
/// NeoForge кладёт файл аргументов, Forge — свой jar, и угадывать имя по версии
/// нельзя: оно менялось между поколениями загрузчика не раз.
fn find_installed_jar(root: &Path) -> Option<String> {
    // Файл аргументов ищем по имени, а не по пути: он лежит под версией
    // загрузчика, а его номер знает установщик, не мы.
    let args = walkdir::WalkDir::new(root.join("libraries"))
        .max_depth(6)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name() == "unix_args.txt")
        .and_then(|e| e.path().strip_prefix(root).ok().map(|p| p.to_path_buf()));
    if let Some(rel) = args {
        return Some(format!("@{}", rel.display()));
    }

    std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| {
            name.ends_with(".jar")
                && (name.contains("forge") || name.contains("server"))
                && !name.contains("installer")
        })
}

fn core_file_name(url: &str) -> String {
    url.rsplit('/')
        .next()
        .filter(|n| n.ends_with(".jar"))
        .unwrap_or("server.jar")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_name_comes_from_the_url() {
        assert_eq!(
            core_file_name("https://fill-data.papermc.io/v1/objects/39bd8c00/paper-1.21.1-133.jar"),
            "paper-1.21.1-133.jar"
        );
        // Ссылка из нашего стора — content-addressed, имени файла в ней нет.
        assert_eq!(
            core_file_name("https://api.example.com/files/abc123"),
            "server.jar"
        );
    }
}
