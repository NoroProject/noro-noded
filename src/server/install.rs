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
    if jar.is_file() {
        return Ok(false);
    }
    let url = format!("{}{AUTHLIB_PATH}", master.base());
    master
        .download(&url, &jar, None, None)
        .await
        .context("не скачать authlib-injector")?;
    Ok(true)
}

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

/// Чем сервер запускается на самом деле.
///
/// Мастер присылает то, что записано в карточке, а правда лежит на диске: имя
/// мог сменить установщик, а карточка — остаться с `server.jar`. Пересобрать
/// контейнер под несуществующий файл значит сломать работающий сервер правкой
/// лимита памяти.
///
/// Если на диске не нашлось ничего похожего, оставляем присланное: отказ при
/// старте с внятной строкой в логе честнее, чем подставленный наугад файл.
pub fn resolve_entry_point(root: &Path, wanted: &str) -> String {
    let exists = match wanted.strip_prefix('@') {
        Some(rel) => root.join(rel).is_file(),
        None => !wanted.is_empty() && root.join(wanted).is_file(),
    };
    if exists {
        return wanted.to_string();
    }
    match find_installed_jar(root) {
        Some(found) => {
            tracing::info!(wanted, found, "точка входа найдена на диске");
            found
        }
        None => wanted.to_string(),
    }
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

    // В корне ищем то, чем сервер вообще может быть: имена у ядер разные —
    // `paper-1.21.1-133.jar`, `fabric-server-launch.jar`, `forge-1.16.5-….jar`,
    // `server.jar`. Установщик и клиентский jar исключаются явно.
    let mut candidates: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            let lower = name.to_lowercase();
            lower.ends_with(".jar") && !lower.contains("installer") && !lower.contains("client")
        })
        .collect();

    // Порядок разбора важен: при нескольких кандидатах выигрывает тот, чьё имя
    // прямо называет себя сервером, а не первый попавшийся в каталоге.
    candidates.sort();
    let rank = |name: &str| {
        let lower = name.to_lowercase();
        match () {
            _ if lower == "server.jar" => 0,
            _ if lower.contains("server") => 1,
            _ if lower.contains("forge") => 2,
            _ if lower.contains("paper") || lower.contains("purpur") => 3,
            _ => 4,
        }
    };
    candidates.into_iter().min_by_key(|name| rank(name))
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

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("noded-entry-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn file(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// То, что на диске есть, не перепроверяется: карточка права.
    #[test]
    fn an_existing_entry_point_is_kept() {
        let d = Dir::new();
        d.file("paper-1.21.1-133.jar");
        assert_eq!(
            resolve_entry_point(&d.0, "paper-1.21.1-133.jar"),
            "paper-1.21.1-133.jar"
        );
    }

    /// Главная проверка файла. У сервера, поставленного до того, как мастер
    /// научился запоминать точку входа, в карточке стоит `server.jar` — и
    /// пересборка контейнера по ней ломала работающий сервер.
    #[test]
    fn a_stale_card_is_corrected_from_the_disk() {
        let d = Dir::new();
        d.file("paper-1.21.1-133.jar");
        assert_eq!(
            resolve_entry_point(&d.0, "server.jar"),
            "paper-1.21.1-133.jar"
        );
    }

    /// У NeoForge в корне вообще нет jar: запускается он файлом аргументов,
    /// который написал установщик.
    #[test]
    fn a_neoforge_args_file_is_found_instead_of_a_jar() {
        let d = Dir::new();
        d.file("libraries/net/neoforged/neoforge/21.1.77/unix_args.txt");
        assert_eq!(
            resolve_entry_point(&d.0, "server.jar"),
            "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt"
        );
    }

    /// Файл аргументов на месте — его и оставляем, не уходя искать заново.
    #[test]
    fn an_existing_args_file_is_kept() {
        let d = Dir::new();
        d.file("libraries/net/neoforged/neoforge/21.1.77/unix_args.txt");
        let wanted = "@libraries/net/neoforged/neoforge/21.1.77/unix_args.txt";
        assert_eq!(resolve_entry_point(&d.0, wanted), wanted);
    }

    /// Установщик остаётся лежать в корне рядом с результатом. Запустить его
    /// вместо сервера — это установка по кругу на каждый старт.
    #[test]
    fn the_installer_is_never_the_entry_point() {
        let d = Dir::new();
        d.file("forge-1.16.5-36.2.39-installer.jar");
        d.file("forge-1.16.5-36.2.39.jar");
        assert_eq!(
            resolve_entry_point(&d.0, "server.jar"),
            "forge-1.16.5-36.2.39.jar"
        );
    }

    /// Ничего похожего на диске нет — присланное остаётся как есть: отказ при
    /// старте с внятной строкой честнее подставленного наугад файла.
    #[test]
    fn nothing_on_disk_leaves_the_card_alone() {
        let d = Dir::new();
        assert_eq!(resolve_entry_point(&d.0, "server.jar"), "server.jar");
    }

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
