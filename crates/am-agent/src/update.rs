//! Installation et mise à jour d'une stack, dans l'ordre exact de docs/PROTOCOLE.md, « Agent
//! `drakbase-stack` » : vérification, téléchargement, signature, extraction, contrôle du compose
//! et des permissions, `.env`, `pull`, préparation, redémarrage, puis succès ou échec.

use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use am_protocol::stack::archive;
use am_protocol::stack::compose::{
    self, ComposeContext, ComposeReport, HostPathKind, HostSource, Permission,
};
use am_protocol::stack::control::{Action, Available, Operation, OperationState, Request};
use am_protocol::stack::manifest::{Rollback, StackManifest};
use am_protocol::update::UpdateResponse;
use am_protocol::{Arch, BundleType, Version, human_size, visible, visible_lines};
use anyhow::{Context, Result, anyhow, bail};

use crate::auto;
use crate::config::Config;
use crate::control;
use crate::docker::{self, Docker, DockerVersions, Project};
use crate::envfile::{self, Merged};
use crate::files;
use crate::http::{Check, Http};
use crate::outcome::{Outcome, describe};
use crate::paths::{Paths, StackPaths};
use crate::prompt;
use crate::state::{self, ReleaseRecord, StackState};
use crate::trust;
use crate::{CheckArgs, InstallArgs, UpdateArgs};

/// Lignes de journal montrées par service après un échec.
const FAILURE_LOG_LINES: u32 = 50;

/// Ce dont toutes les commandes ont besoin.
pub struct Agent {
    pub paths: Paths,
    pub docker: Docker,
    /// Terminal à l'entrée et à la sortie d'erreur : les questions sont permises.
    pub interactive: bool,
}

impl Agent {
    pub fn from_env(interactive: bool) -> Self {
        Self {
            paths: Paths::from_env(),
            docker: Docker::from_env(),
            interactive,
        }
    }
}

/// Arch DrakBase de la machine, celle de cet exécutable.
pub fn machine_arch() -> Result<Arch> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(Arch::X86_64),
        "aarch64" => Ok(Arch::Aarch64),
        "arm" => Ok(Arch::Armv7),
        "riscv64" => Ok(Arch::Riscv64),
        "x86" => Ok(Arch::I686),
        other => bail!("architecture {other} non prise en charge"),
    }
}

/// Les messages de l'agent portent du texte venu de la stack ou du serveur : il est rendu visible
/// avant de partir au terminal (`am_protocol::visible`).
pub fn say(app: &str, message: impl Display) {
    println!("{app} : {}", visible_lines(&message.to_string()));
}

fn warn(app: &str, message: impl Display) {
    eprintln!("{app} : {}", visible_lines(&message.to_string()));
}

/// Un élément d'une liste à lire avant d'accepter, rendu visible : une permission, par exemple.
pub fn list_item(item: impl Display) {
    println!("  - {}", visible(&item.to_string()));
}

struct Options {
    accept: bool,
    retry: bool,
    install: bool,
}

struct Session<'a> {
    agent: &'a Agent,
    config: Config,
    versions: DockerVersions,
    arch: Arch,
    http: Http,
}

impl<'a> Session<'a> {
    /// Ce qu'il faut à toute opération sur une stack installée : Docker contrôlé, la
    /// configuration de la machine, son arch et le client du serveur.
    fn open(agent: &'a Agent) -> Result<Self> {
        Ok(Self {
            agent,
            versions: agent.docker.prerequisites()?,
            config: Config::load_or_create(&agent.paths, None)?,
            arch: machine_arch()?,
            http: Http::new()?,
        })
    }
}

/// Les stacks qu'une commande vise : celle qui est nommée, qui doit être installée, sinon toutes.
fn targets(agent: &Agent, app: Option<&String>) -> Result<Vec<String>> {
    match app {
        Some(app) => {
            StackState::require(&agent.paths, app)?;
            Ok(vec![app.clone()])
        }
        None => state::list(&agent.paths),
    }
}

/// Comment accepter, sur la machine, les permissions nouvelles d'une version.
fn accept_hint(app: &str) -> String {
    format!("à accepter sur la machine : sudo drakbase-stack update {app} --accept")
}

pub fn install(agent: &Agent, args: InstallArgs) -> Result<Outcome> {
    let app = args.app.as_str();
    let versions = agent.docker.prerequisites()?;
    let arch = machine_arch()?;
    let stack = agent.paths.stack(app);
    let dir_existed = stack.dir.exists();
    let env_existed = stack.env_file().exists();
    let _lock = files::lock(&stack.lock_file(), app)?;
    let result = install_locked(agent, &args, versions, arch, &stack);

    if !stack.state_file().exists() {
        // L'installation n'a rien démarré : elle ne laisse rien derrière elle, sauf un .env
        // gardé d'une installation précédente.
        let _ = files::remove_dir(&stack.releases());
        // Créé avant `pull`, pour le montage d'une stack qui a `app_control`.
        let _ = files::remove_dir(&stack.control_dir());
        let _ = files::remove_file(&stack.env_next());
        if !env_existed {
            let _ = files::remove_file(&stack.env_file());
        }
        if !dir_existed {
            let _ = files::remove_file(&stack.lock_file());
            let _ = fs::remove_dir(&stack.dir);
        }
    } else if matches!(result, Ok(Outcome::Done)) && !auto::is_enabled(&agent.paths) {
        println!(
            "Pour que {app} vérifie chaque jour ses mises à jour et reçoive les demandes de son interface : sudo drakbase-stack auto on"
        );
    }
    result
}

fn install_locked(
    agent: &Agent,
    args: &InstallArgs,
    versions: DockerVersions,
    arch: Arch,
    stack: &StackPaths,
) -> Result<Outcome> {
    let app = args.app.as_str();
    if stack.state_file().exists() {
        bail!(
            "{app} est déjà installée : drakbase-stack update {app} pour la mettre à jour, drakbase-stack uninstall {app} pour la retirer"
        );
    }
    if agent.docker.projects()?.iter().any(|name| name == app) {
        bail!(
            "un projet Docker Compose « {app} » existe déjà sur cette machine, sans avoir été créé par drakbase-stack : arrêtez-le d'abord (docker compose -p {app} down, sans --volumes si ses données doivent être reprises)"
        );
    }

    let mut config = Config::load_or_create(&agent.paths, args.server.as_ref())?;
    if args.no_stats && config.stats {
        config.stats = false;
        config.save(&agent.paths)?;
    }
    let server = args.server.clone().unwrap_or_else(|| config.server.clone());
    if !server.is_https() && !server.is_loopback() {
        warn(
            app,
            format!("attention : {server} est en http, sans chiffrement"),
        );
    }
    let mut state = StackState::new(app, server, args.channel, args.key.clone());
    let session = Session {
        agent,
        config,
        versions,
        arch,
        http: Http::new()?,
    };
    let options = Options {
        accept: args.accept,
        retry: false,
        install: true,
    };
    apply(&session, &mut state, &options)
}

pub fn update(agent: &Agent, args: UpdateArgs) -> Result<Outcome> {
    let apps = targets(agent, args.app.as_ref())?;
    if apps.is_empty() {
        println!("Aucune stack installée.");
        return Ok(Outcome::Done);
    }
    let session = Session::open(agent)?;
    let options = Options {
        accept: args.accept,
        retry: args.retry,
        install: false,
    };
    if args.app.is_some() {
        return update_one(&session, &apps[0], &options);
    }
    let mut outcome = Outcome::Done;
    for app in &apps {
        let result = update_one(&session, app, &options).unwrap_or_else(|error| {
            eprintln!("erreur : {app} : {}", describe(&error));
            Outcome::Failed
        });
        outcome = outcome.worst(result);
    }
    Ok(outcome)
}

fn update_one(session: &Session<'_>, app: &str, options: &Options) -> Result<Outcome> {
    let stack = session.agent.paths.stack(app);
    let _lock = files::lock(&stack.lock_file(), app)?;
    let mut state = StackState::require(&session.agent.paths, app)?;
    mark_interrupted(&session.agent.paths, &mut state)?;
    apply(session, &mut state, options)
}

/// Défait ce qu'une mise à jour a préparé quand elle s'arrête avant d'aboutir : le dossier où la
/// version a été examinée, son dossier une fois en place, le `.env` préparé, le `.env` complété
/// (celui d'avant revient) et le dossier de contrôle créé pour elle.
struct Pending {
    /// Dossier où la version est extraite et examinée.
    examined_dir: PathBuf,
    /// Dossier de la version une fois en place, s'il n'est pas `examined_dir`.
    release_dir: Option<PathBuf>,
    env_next: PathBuf,
    /// Le dossier de la version reste : elle a démarré, ou c'est la version restée en échec, dont
    /// les conteneurs tournent peut-être encore.
    keep_release: bool,
    /// Le `.env` d'avant l'étape 6, tant que la version n'a pas abouti.
    env_before: Option<EnvBefore>,
    /// La stack dont le dossier de contrôle a été créé pour cette version, tant qu'elle n'a pas
    /// démarré : la version qui tourne n'en a pas.
    new_control: Option<StackPaths>,
}

impl Pending {
    /// Ce qu'il faut retirer d'un examen sans installation, ou avant l'étape 5 d'une mise à jour.
    fn new(examined_dir: PathBuf, release_dir: Option<PathBuf>, env_next: PathBuf) -> Self {
        Self {
            examined_dir,
            release_dir,
            env_next,
            keep_release: false,
            env_before: None,
            new_control: None,
        }
    }

    /// Remet le `.env` d'avant l'étape 6, si la version l'a complété.
    fn restore_env(&mut self) {
        if let Some(before) = self.env_before.take() {
            before.restore();
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.restore_env();
        if let Some(stack) = self.new_control.take() {
            let _ = control::remove(&stack);
        }
        let _ = files::remove_dir(&self.examined_dir);
        if let Some(release_dir) = &self.release_dir
            && !self.keep_release
        {
            let _ = files::remove_dir(release_dir);
        }
        let _ = files::remove_file(&self.env_next);
    }
}

/// Le `.env` tel qu'il était avant qu'une version ne le complète (étape 6). Il revient quand la
/// version n'aboutit pas : la version gardée, si elle est relancée, retrouve les réglages qu'elle
/// avait, et pas ceux qu'une version suivante a choisis pour elle.
struct EnvBefore {
    app: String,
    file: PathBuf,
    /// `None` : il n'existait pas, et il est retiré.
    content: Option<Vec<u8>>,
    /// Le `.env` écrit à l'étape 6. Changé depuis, à la main, il n'est pas remis tel quel.
    written: Vec<u8>,
    /// Réglages que la version y a ajoutés.
    added: Vec<String>,
}

impl EnvBefore {
    /// Remet le `.env` d'avant, octet pour octet, s'il est encore celui écrit à l'étape 6. Changé
    /// entre-temps (à la main, pendant l'attente de l'état sain), il garde ces changements :
    /// seuls les réglages que la version avait ajoutés en sont retirés, et l'agent le dit.
    fn restore(self) {
        let current = match read_optional_bytes(&self.file) {
            Ok(current) => current,
            Err(error) => {
                warn(
                    &self.app,
                    format!(
                        "attention : le .env d'avant n'a pas pu être remis : {}",
                        describe(&error)
                    ),
                );
                return;
            }
        };
        if current.as_deref() != Some(self.written.as_slice()) {
            self.restore_changed(current);
            return;
        }
        let restored = match &self.content {
            Some(content) => files::replace(&self.file, content, 0o600),
            None => files::remove_file(&self.file),
        };
        match restored {
            Ok(()) if self.content.is_some() && !self.added.is_empty() => say(
                &self.app,
                format!(
                    ".env remis comme avant ; réglages retirés : {}",
                    self.added.join(", ")
                ),
            ),
            Ok(()) => {}
            Err(error) => warn(
                &self.app,
                format!(
                    "attention : le .env d'avant n'a pas pu être remis : {}",
                    describe(&error)
                ),
            ),
        }
    }

    /// Le `.env` a changé depuis l'étape 6 : seuls les réglages ajoutés par la version partent.
    fn restore_changed(self, current: Option<Vec<u8>>) {
        let file = self.file.display();
        let Some(current) = current else {
            warn(
                &self.app,
                format!(
                    "attention : {file} a été retiré pendant la mise à jour ; il n'est pas remis"
                ),
            );
            return;
        };
        let Ok(text) = std::str::from_utf8(&current) else {
            warn(
                &self.app,
                format!(
                    "attention : {file} a changé pendant la mise à jour et n'est pas en UTF-8 : laissé tel quel, réglages ajoutés compris ({})",
                    self.added.join(", ")
                ),
            );
            return;
        };
        let (kept, removed) = envfile::remove(text, &self.added);
        if removed.is_empty() {
            say(
                &self.app,
                format!("{file} a changé pendant la mise à jour : laissé tel quel"),
            );
            return;
        }
        match files::replace(&self.file, kept.as_bytes(), 0o600) {
            Ok(()) => say(
                &self.app,
                format!(
                    "{file} a changé pendant la mise à jour : vos changements restent, seuls les réglages ajoutés par la version en sont retirés : {}",
                    removed.join(", ")
                ),
            ),
            Err(error) => warn(
                &self.app,
                format!(
                    "attention : les réglages ajoutés n'ont pas pu être retirés de {file} : {}",
                    describe(&error)
                ),
            ),
        }
    }
}

/// Comment les étapes 2 à 10 ont fini.
struct Applied {
    outcome: Outcome,
    /// Ce que l'issue seule ne dit pas à l'interface de l'app : pourquoi le retour seul à la
    /// version installée ne s'est pas fait, et la commande à taper.
    blocked_return: Option<String>,
}

impl From<Outcome> for Applied {
    fn from(outcome: Outcome) -> Self {
        Self {
            outcome,
            blocked_return: None,
        }
    }
}

/// Étape 1, la vérification : la version plus récente que propose le serveur, ou rien. Une
/// version qui n'est pas plus récente, ou une bêta pour le canal stable, est une incohérence du
/// serveur : c'est « à jour ».
fn offer(session: &Session<'_>, state: &StackState) -> Result<Option<UpdateResponse>> {
    let zero = Version::parse_release("0.0.0")?;
    let installed = state.installed_version().cloned().unwrap_or(zero);
    let offer = session.http.check(&Check {
        server: &state.server,
        app: &state.app,
        arch: session.arch,
        version: &installed,
        bundle_type: BundleType::Stack,
        channel: state.channel,
        os_version: Some(&session.versions.compose),
        install_id: session.config.install_id(),
    })?;
    Ok(offer.filter(|offer| offer.version > installed && state.channel.includes(offer.channel)))
}

/// Ce que les étapes 2 à 5 ont trouvé.
struct Examined {
    manifest: StackManifest,
    report: ComposeReport,
    config_json: Vec<u8>,
    current_env: Option<String>,
    merged: Merged,
}

/// Étapes 2 à 5 : téléchargement, signature, extraction dans `examined_dir` et contrôle du
/// compose, avec le `.env` qu'aurait la version, préparé dans `env_next`. Rien n'est installé, et
/// le dossier de la version n'est pas touché.
fn examine(
    session: &Session<'_>,
    state: &StackState,
    offer: &UpdateResponse,
    examined_dir: &Path,
    env_next: &Path,
) -> Result<Examined> {
    let app = state.app.as_str();
    let stack = session.agent.paths.stack(app);
    let version = &offer.version;

    // 2. Téléchargement.
    if offer.size > archive::MAX_ARCHIVE_BYTES {
        bail!(
            "la version {version} annonce une archive de {}, bien plus qu'une stack",
            human_size(offer.size)
        );
    }
    say(
        app,
        format!(
            "version {version} : téléchargement ({})",
            human_size(offer.size)
        ),
    );
    files::create_dir(&stack.releases())?;
    let partial = stack.partial();
    session
        .http
        .download(
            &state.server,
            &offer.url,
            &partial,
            offer.size,
            &offer.sha256,
        )
        .with_context(|| format!("version {version} refusée"))?;
    let bytes = fs::read(&partial);
    let _ = fs::remove_file(&partial);
    let bytes = bytes.with_context(|| format!("lecture de {}", partial.display()))?;

    // 3. Signature, avec les clés de confiance ; la version signée est celle annoncée, plus
    // haute que celle installée (contrôlé à l'étape 1).
    let key_id = trust::verify(&state.trusted_keys, &bytes, &offer.signature, version)
        .with_context(|| format!("version {version} refusée"))?;
    say(app, format!("signature vérifiée (clé {key_id})"));

    // 4. Extraction.
    let stack_archive = archive::read(&bytes)
        .map_err(|error| anyhow!("version {version} refusée : {error} ({})", error.code()))?;
    let manifest = StackManifest::parse_published(stack_archive.stack_json(), app, version)
        .map_err(|error| anyhow!("version {version} refusée : {error} ({})", error.code()))?;
    if let Some(minimum) = &manifest.min_compose_version
        && !docker::at_least(&session.versions.compose, minimum)
    {
        bail!(
            "la version {version} demande Docker Compose {minimum} ou plus, et cette machine a {}",
            session.versions.compose
        );
    }
    // Le compose de l'archive est contrôlé tel qu'il est écrit, avant que Docker Compose ne le
    // lise : sa forme résolue ne dit plus quels fichiers de la machine il a lus.
    compose::check_archived(stack_archive.compose_yaml()).map_err(|error| {
        anyhow!(
            "compose de la version {version} refusé : {error} ({})",
            error.code()
        )
    })?;
    // Ses réglages aussi, avant d'entrer dans le .env de la machine.
    if let Some(env_example) = stack_archive.env_example() {
        compose::check_env_example(env_example).map_err(|error| {
            anyhow!(
                "réglages de la version {version} refusés : {error} ({})",
                error.code()
            )
        })?;
    }
    // Reste d'un essai précédent, interrompu avant la fin.
    files::remove_dir(examined_dir)?;
    if let Some(parent) = examined_dir.parent() {
        files::create_dir(parent)?;
    }
    files::extract(stack_archive.files(), examined_dir)?;

    // 5. Contrôle du compose, avec le .env qu'aura la version (étape 6), pour que les
    // permissions soient celles qui démarreront.
    let current_env = read_optional(&stack.env_file())?;
    let example = stack_archive
        .env_example()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
    let merged = envfile::merge(current_env.as_deref(), example.as_deref());
    files::replace(env_next, merged.text.as_bytes(), 0o600)?;
    let config_json = session
        .agent
        .docker
        .config(&Project {
            name: app,
            dir: examined_dir,
            env_file: env_next,
        })
        .with_context(|| format!("version {version} refusée"))?;
    let report = analyze_compose(state, &manifest, examined_dir, &config_json)
        .with_context(|| format!("compose de la version {version} refusé"))?;
    Ok(Examined {
        manifest,
        report,
        config_json,
        current_env,
        merged,
    })
}

/// Contrôle le compose résolu d'une version, dans son dossier : les règles d'une stack, et les
/// permissions qu'elle demande (docs/PROTOCOLE.md, « Compose »).
pub fn analyze_compose(
    state: &StackState,
    manifest: &StackManifest,
    project_dir: &Path,
    config_json: &[u8],
) -> Result<ComposeReport> {
    let dir = project_dir
        .to_str()
        .ok_or_else(|| anyhow!("{} : chemin non UTF-8", project_dir.display()))?;
    let context = ComposeContext {
        app: &state.app,
        host: state.server.image_host(),
        project_dir: dir,
        service: &manifest.service,
        pre_update: manifest.pre_update.as_deref(),
        images: &manifest.images,
    };
    let report = compose::analyze(config_json, &context)
        .map_err(|error| anyhow!("{error} ({})", error.code()))?;
    check_links(config_json, dir)?;
    Ok(report)
}

/// Les règles du compose jugent le texte d'un chemin. Sur la machine, une source de montage peut
/// être un lien, ou en traverser un : celle qui mène ainsi à un dossier de l'agent, à la socket
/// d'un moteur de conteneurs ou, en écriture, à un dossier du système, sans que son chemin le
/// dise, est refusée. Une source absente n'est pas un lien : Docker la créera.
fn check_links(config_json: &[u8], project_dir: &str) -> Result<()> {
    for HostSource {
        path: source,
        writable,
    } in compose::host_sources(config_json, project_dir)
    {
        let Ok(real) = fs::canonicalize(&source) else {
            continue;
        };
        let real = real.to_string_lossy();
        let reached = compose::host_path_kind(&real);
        if reached == compose::host_path_kind(&source) {
            continue;
        }
        match reached {
            HostPathKind::AgentDir => bail!(
                "« {source} » mène, par un lien de la machine, à {real} : un dossier de drakbase-stack, qu'aucune stack ne monte (compose_value_unsupported)"
            ),
            HostPathKind::EngineSocket => bail!(
                "« {source} » mène, par un lien de la machine, à {real} : la socket d'un moteur de conteneurs. Elle se monte sous son vrai chemin, pour que la permission le dise (compose_value_unsupported)"
            ),
            // En lecture seule, la permission montrée ne distingue pas un dossier du système.
            HostPathKind::SystemDir if writable => bail!(
                "« {source} » mène, par un lien de la machine, à {real} : un dossier du système, monté en écriture. Il se monte sous son vrai chemin, pour que la permission le dise (compose_value_unsupported)"
            ),
            HostPathKind::SystemDir | HostPathKind::Plain => {}
        }
    }
    Ok(())
}

/// Ce qui reste en place quand une version s'arrête avant de démarrer : les conteneurs de la
/// version restée en échec, sinon la version installée ; rien avant une première installation.
fn in_place(state: &StackState) -> Option<String> {
    match (&state.failed, state.installed_version()) {
        (Some(failed), _) => Some(format!(
            "les conteneurs de la version {} restent en l'état",
            failed.version
        )),
        (None, Some(installed)) => Some(format!("la version {installed} continue de tourner")),
        (None, None) => None,
    }
}

/// Les étapes 1 à 10, pour une installation (version installée : 0.0.0) ou une mise à jour.
fn apply(session: &Session<'_>, state: &mut StackState, options: &Options) -> Result<Outcome> {
    // 1. Vérification.
    let Some(offer) = offer(session, state)? else {
        return up_to_date(session, state, options);
    };
    apply_offer(session, state, options, &offer).map(|applied| applied.outcome)
}

/// Les étapes 2 à 10, pour la version que la vérification vient de proposer.
fn apply_offer(
    session: &Session<'_>,
    state: &mut StackState,
    options: &Options,
    offer: &UpdateResponse,
) -> Result<Applied> {
    let app = state.app.clone();
    let app = app.as_str();
    let docker = &session.agent.docker;
    let stack = session.agent.paths.stack(app);
    let version = offer.version.clone();
    if state.is_rejected(&version) && !options.retry {
        say(
            app,
            format!(
                "la version {version} a été écartée, après un échec ou un retour arrière : drakbase-stack update {app} --retry pour la reprendre"
            ),
        );
        return Ok(failed_reminder(session.agent, state).into());
    }
    // Une version en échec a pu changer le format des données : une version plus ancienne ne
    // s'installe pas par-dessus sans que l'utilisateur soit d'abord revenu en arrière.
    if let Some(failed) = &state.failed
        && version < failed.version
    {
        warn(
            app,
            format!(
                "la version {version} est plus ancienne que la {}, restée en échec : sudo drakbase-stack rollback {app} d'abord",
                failed.version
            ),
        );
        return Ok(Outcome::Failed.into());
    }
    // 2 à 5, dans un dossier à côté de celui de la version, qui n'est touché qu'ensuite : une
    // reprise qui s'arrête ici laisse intacte la version restée en échec.
    let release_dir = stack.release(&version);
    let staging = stack.staging(&version);
    let resumes_failed = state
        .failed
        .as_ref()
        .is_some_and(|failed| failed.version == version);
    let mut pending = Pending::new(staging.clone(), Some(release_dir.clone()), stack.env_next());
    pending.keep_release = resumes_failed;
    let Examined {
        manifest,
        report,
        config_json,
        current_env,
        merged,
    } = examine(session, state, offer, &staging, &stack.env_next())?;
    for warning in &report.warnings {
        warn(app, format!("attention : {warning}"));
    }
    let new_permissions = state.new_permissions(&report);
    if !new_permissions.is_empty() {
        if !accept_permissions(session, state, &new_permissions, options, &version)? {
            return Ok(Outcome::PermissionRequired.into());
        }
    } else if options.install {
        say(app, "aucune permission particulière demandée");
    }
    let images = docker::images_of(&config_json);
    // La version examinée prend sa place, par renommage : un dossier du même nom (une version
    // restée en échec que l'on reprend, le reste d'un essai interrompu) est remplacé d'un coup.
    files::replace_dir(&staging, &release_dir)?;
    // Le montage du dossier de contrôle, si l'app propose ses mises à jour dans son interface.
    // Créé pour cette version, il part si elle s'arrête avant de démarrer.
    if manifest.app_control.is_some() && !control::exists(&stack) {
        pending.new_control = Some(stack.clone());
    }
    control::write_compose(&stack, &release_dir, &manifest)?;

    // 6. .env : les variables nouvelles de .env.example y entrent, les autres ne changent pas.
    // Celui d'avant est gardé de côté : il revient si la version n'aboutit pas.
    let env_before = read_optional_bytes(&stack.env_file())?;
    fs::rename(stack.env_next(), stack.env_file())
        .with_context(|| format!("écriture de {}", stack.env_file().display()))?;
    pending.env_before = Some(EnvBefore {
        app: app.to_owned(),
        file: stack.env_file(),
        content: env_before,
        written: merged.text.as_bytes().to_vec(),
        added: merged.added.clone(),
    });
    if current_env.is_none() {
        say(
            app,
            format!(
                "réglages dans {} (créé à partir de .env.example)",
                stack.env_file().display()
            ),
        );
    } else {
        for name in &merged.added {
            say(app, format!("réglage ajouté à .env : {name}"));
        }
    }
    let project = Project {
        name: app,
        dir: &release_dir,
        env_file: &stack.env_file(),
    };

    // 7. pull : Docker ne télécharge que les couches qui lui manquent.
    let in_place = in_place(state).unwrap_or_else(|| "rien n'a été démarré".to_owned());
    say(app, "téléchargement des images (docker compose pull)");
    docker
        .pull(&project)
        .with_context(|| format!("images de la version {version} indisponibles ; {in_place}"))?;

    // 8. Préparation, avec la nouvelle version. Une première installation n'a rien à préparer.
    if let Some(hook) = &manifest.pre_update
        && state.installed.is_some()
    {
        say(app, format!("préparation : service {hook}"));
        docker
            .run_hook(&project, hook)
            .with_context(|| format!("préparation de la version {version} ; {in_place}"))?;
    }

    // 9. Redémarrage.
    if options.install {
        // À partir d'ici, des conteneurs existent : l'état est gardé, même en cas d'échec.
        state.save(&session.agent.paths)?;
    }
    say(
        app,
        format!(
            "démarrage, attente de l'état sain ({} s au plus)",
            manifest.health_timeout
        ),
    );
    let record = ReleaseRecord::new(version.clone(), images);
    // Les conteneurs de la version montent le dossier de contrôle dès qu'ils existent.
    pending.new_control = None;
    match docker.up(&project, manifest.health_timeout) {
        Ok(()) => succeed(session, state, record, &manifest, &mut pending).map(Applied::from),
        Err(error) => {
            warn(
                app,
                format!("la version {version} n'est pas devenue saine : {error}"),
            );
            fail(session, state, record, &manifest, &mut pending, &project)
        }
    }
}

/// Étape 10, succès : la version devient l'installée, l'ancienne la précédente ; les plus
/// anciennes partent avec leurs images, et les clés de confiance deviennent celles de la
/// nouvelle version.
fn succeed(
    session: &Session<'_>,
    state: &mut StackState,
    record: ReleaseRecord,
    manifest: &StackManifest,
    pending: &mut Pending,
) -> Result<Outcome> {
    pending.keep_release = true;
    // La version a démarré avec le .env complété : il reste.
    pending.env_before = None;
    let version = record.version.clone();
    let mut dropped = Vec::new();
    let left = state.installed.replace(record);
    dropped.extend(state.previous.take());
    state.previous = left;
    if let Some(failed) = state.failed.take()
        && failed.version != version
    {
        dropped.push(failed);
    }
    // Une version écartée reste écartée tant qu'un `rollback` peut ramener sous elle : seules
    // celles plus anciennes que la précédente gardée sont oubliées.
    let floor = state
        .previous
        .as_ref()
        .map(|previous| previous.version.clone());
    state.rejected.retain(|rejected| {
        rejected != &version && floor.as_ref().is_none_or(|floor| rejected > floor)
    });
    state.trusted_keys = manifest.public_keys.clone();
    state.forget_available_up_to(&version);
    settle_control(session.agent, &state.app, manifest.app_control.is_some());
    state.save(&session.agent.paths)?;
    cleanup(session.agent, state, &dropped);
    let app = &state.app;
    match &state.previous {
        Some(previous) => say(
            app,
            format!(
                "version {version} en place (la {} reste gardée pour drakbase-stack rollback {app})",
                previous.version
            ),
        ),
        None => say(app, format!("version {version} en place")),
    }
    Ok(Outcome::Done)
}

/// Échec à l'étape 9 : retour seul avec `rollback: safe`, arrêt et explication sinon. Dans les
/// deux cas, la version est écartée, et le `.env` d'avant revient.
fn fail(
    session: &Session<'_>,
    state: &mut StackState,
    record: ReleaseRecord,
    manifest: &StackManifest,
    pending: &mut Pending,
    project: &Project<'_>,
) -> Result<Applied> {
    let app = state.app.clone();
    let app = app.as_str();
    let docker = &session.agent.docker;
    let stack = session.agent.paths.stack(app);
    let version = record.version.clone();
    state.reject(&version);
    let added: Vec<String> = pending
        .env_before
        .as_ref()
        .map(|before| before.added.clone())
        .unwrap_or_default();

    // Le retour seul ne s'est pas fait, faute de pouvoir accepter une permission.
    let mut return_needs_accept = false;
    // La version installée ne peut plus être relancée : son compose est refusé (avec ce `.env`
    // ou non). `rollback` échouerait de même.
    let mut installed_refused = None;
    if let (Rollback::Safe, Some(installed)) = (manifest.rollback, state.installed.clone()) {
        // La version installée repart avec les réglages qu'elle avait.
        pending.restore_env();
        say(
            app,
            format!(
                "retour à la version {} (rollback : safe)",
                installed.version
            ),
        );
        match examine_kept(session.agent, state, &installed.version) {
            Ok(kept) if kept.new_permissions.is_empty() => {
                return return_alone(session, state, record, &installed, &kept.manifest)
                    .map(Applied::from);
            }
            // Le retour seul n'accepte rien : seule une personne, sur la machine, le peut.
            Ok(kept) => {
                println!(
                    "{app} : avec les réglages de .env, la version {} demande de nouvelles permissions :",
                    installed.version
                );
                for permission in &kept.new_permissions {
                    list_item(permission);
                }
                println!("Le retour seul ne les accepte pas : rien n'a été relancé.");
                return_needs_accept = true;
            }
            Err(error) => {
                installed_refused = kept_refused(&error);
                warn(
                    app,
                    format!(
                        "la version {} n'a pas été relancée : {}",
                        installed.version,
                        describe(&error)
                    ),
                );
            }
        }
    }

    // Les conteneurs restent en l'état : la nouvelle version peut avoir changé le format des
    // données, et seul l'utilisateur peut décider du retour. C'est aussi le cas d'un retour seul
    // qui ne s'est pas fait.
    pending.keep_release = true;
    let mut dropped = Vec::new();
    if let Some(previous_failure) = state.failed.replace(record)
        && previous_failure.version != version
    {
        dropped.push(previous_failure);
    }
    state.save(&session.agent.paths)?;
    cleanup(session.agent, state, &dropped);
    show_logs(docker, project);
    let Some(installed) = state.installed_version().cloned() else {
        // Une première installation n'a pas de version à relancer : ses conteneurs gardent le
        // .env avec lequel ils ont démarré.
        pending.env_before = None;
        println!(
            "Les conteneurs de la version {version} sont laissés en l'état. Pour tout arrêter et retirer :\n  sudo drakbase-stack uninstall {app}"
        );
        println!(
            "La version {version} est écartée ; pour la reprendre : sudo drakbase-stack update {app} --retry"
        );
        return Ok(Outcome::Failed.into());
    };
    // Le .env d'avant revient : c'est celui de la version que `rollback` relancerait.
    pending.restore_env();
    // Avec `rollback: manual`, la version installée n'a pas encore été contrôlée : elle l'est,
    // pour ne pas conseiller un `rollback` qui serait refusé.
    if matches!(manifest.rollback, Rollback::Manual)
        && let Err(error) = examine_kept(session.agent, state, &installed)
    {
        installed_refused = kept_refused(&error);
    }
    if let Some(with_env) = installed_refused {
        println!(
            "Les conteneurs de la version {version} sont laissés en l'état. La version {installed} a été refusée : son compose n'est plus admis."
        );
        println!(
            "{}",
            refused_way_out(&stack, app, Some(&version), with_env, "rollback")
        );
    } else {
        let (accept, flag) = if return_needs_accept {
            (", en acceptant ces permissions", " --accept")
        } else {
            ("", "")
        };
        println!(
            "Les conteneurs de la version {version} sont laissés en l'état. Pour revenir à la version {installed}{accept} :\n  sudo drakbase-stack rollback {app}{flag}"
        );
        println!(
            "La version {version} est écartée ; pour la reprendre : sudo drakbase-stack update {app} --retry"
        );
    }
    if !added.is_empty() {
        println!(
            "Ses réglages nouveaux ({}) reviendront alors avec leur valeur par défaut ; pour en choisir une autre, ajoutez-les d'abord à {}.",
            added.join(", "),
            stack.env_file().display()
        );
    }
    if installed_refused.is_some() {
        return Ok(Applied {
            outcome: Outcome::Failed,
            blocked_return: Some(format!(
                "la version {version} n'est pas devenue saine, et la {installed} ne peut plus être relancée : son compose n'est plus admis par cet agent ; ses conteneurs sont restés en l'état. À taper sur la machine : sudo drakbase-stack update {app} --retry pour reprendre la {version}, ou sudo drakbase-stack uninstall {app}"
            )),
        });
    }
    if !return_needs_accept {
        return Ok(Outcome::Failed.into());
    }
    Ok(Applied {
        outcome: Outcome::PermissionRequired,
        blocked_return: Some(format!(
            "la version {version} n'est pas devenue saine, et le retour à la {installed} demande de nouvelles permissions ; ses conteneurs sont restés en l'état. À taper sur la machine : sudo drakbase-stack rollback {app} --accept"
        )),
    })
}

/// Le retour seul (`rollback: safe`) : la version installée, déjà contrôlée, est relancée, et la
/// version qui vient d'échouer part avec ses images.
fn return_alone(
    session: &Session<'_>,
    state: &mut StackState,
    record: ReleaseRecord,
    installed: &ReleaseRecord,
    installed_manifest: &StackManifest,
) -> Result<Outcome> {
    let app = state.app.clone();
    let app = app.as_str();
    let version = record.version.clone();
    let mut dropped = vec![record];
    match relaunch(session.agent, app, &installed.version, installed_manifest) {
        Ok(()) => {
            dropped.extend(state.failed.take());
            say(
                app,
                format!("version {} relancée, saine", installed.version),
            );
        }
        Err(error) => {
            warn(
                app,
                format!(
                    "la version {} ne redevient pas saine non plus : {}",
                    installed.version,
                    describe(&error)
                ),
            );
            show_logs(
                &session.agent.docker,
                &release_project(session.agent, app, &installed.version).project(),
            );
            // Ses conteneurs sont en place, même s'ils ne sont pas sains : le dossier de contrôle
            // et l'unité des demandes la suivent, pas la version écartée.
            settle_control(session.agent, app, installed_manifest.app_control.is_some());
        }
    }
    state.save(&session.agent.paths)?;
    // Le dossier de la version écartée et ses images partent ici.
    cleanup(session.agent, state, &dropped);
    say(
        app,
        format!(
            "la version {version} est écartée : drakbase-stack update {app} --retry pour la reprendre"
        ),
    );
    Ok(Outcome::Failed)
}

pub fn show_logs(docker: &Docker, project: &Project<'_>) {
    println!("Fin du journal ({FAILURE_LOG_LINES} lignes par service) :");
    // Le journal des conteneurs, écrit par la stack : rendu visible lui aussi.
    print!(
        "{}",
        visible_lines(&docker.logs_tail(project, FAILURE_LOG_LINES))
    );
    println!();
}

/// Projet Compose d'une version gardée.
pub fn release_project<'a>(agent: &Agent, app: &'a str, version: &Version) -> OwnedProject<'a> {
    let stack = agent.paths.stack(app);
    OwnedProject {
        name: app,
        dir: stack.release(version),
        env_file: stack.env_file(),
    }
}

/// Un [`Project`] qui possède ses chemins.
pub struct OwnedProject<'a> {
    pub name: &'a str,
    pub dir: PathBuf,
    pub env_file: PathBuf,
}

impl OwnedProject<'_> {
    pub fn project(&self) -> Project<'_> {
        Project {
            name: self.name,
            dir: &self.dir,
            env_file: &self.env_file,
        }
    }
}

/// Une version gardée, contrôlée avant sa relance.
pub struct Kept {
    pub manifest: StackManifest,
    /// Permissions que demande sa forme résolue avec le `.env` courant, et qui ne sont pas encore
    /// acceptées.
    pub new_permissions: Vec<Permission>,
}

/// Une version gardée que cet agent ne relance pas : son compose est refusé, tel qu'il est écrit
/// (`with_env` faux) ou dans sa forme résolue avec le `.env` courant. Une version installée par un
/// agent plus ancien peut employer un champ qu'il admettait et que celui-ci refuse.
#[derive(Debug)]
pub struct KeptRefused {
    pub version: Version,
    pub reason: String,
    pub with_env: bool,
}

impl Display for KeptRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env = if self.with_env { " avec ce .env" } else { "" };
        write!(
            f,
            "compose de la version {} refusé{env} : {}",
            self.version, self.reason
        )
    }
}

impl std::error::Error for KeptRefused {}

/// L'erreur dit-elle qu'une version gardée est refusée ? Rend alors `with_env`.
pub fn kept_refused(error: &anyhow::Error) -> Option<bool> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<KeptRefused>())
        .map(|refused| refused.with_env)
}

/// Ce qui reste possible quand une version gardée est refusée : `rollback` ne la relancera pas.
/// `failed` est la version restée en échec, que `update --retry` reprend ; `command`, la commande à
/// retaper une fois un réglage du `.env` corrigé.
pub fn refused_way_out(
    stack: &StackPaths,
    app: &str,
    failed: Option<&Version>,
    with_env: bool,
    command: &str,
) -> String {
    let mut ways = vec![format!(
        "une version plus récente, dès qu'elle sera publiée : sudo drakbase-stack update {app}"
    )];
    if let Some(failed) = failed {
        ways.push(format!(
            "reprendre la version {failed} : sudo drakbase-stack update {app} --retry"
        ));
    }
    if with_env {
        ways.push(format!(
            "si c'est un réglage de {} qui est refusé, le corriger, puis : sudo drakbase-stack {command} {app}",
            stack.env_file().display()
        ));
    }
    ways.push(format!(
        "tout retirer, en gardant les volumes : sudo drakbase-stack uninstall {app}"
    ));
    let mut text =
        "Cette version de drakbase-stack ne la relancera pas. Ce qui reste possible :".to_owned();
    for way in ways {
        text.push_str("\n  - ");
        text.push_str(&way);
    }
    text
}

/// Contrôle une version gardée avant de la relancer, comme l'étape 5 le fait d'une version qui
/// arrive : la forme résolue de son seul compose avec le `.env` courant (`docker compose
/// config`), les règles d'une stack, puis ses permissions, comparées à celles déjà acceptées. Le
/// `.env` a pu changer depuis son dernier démarrage : à la main, ou par une version plus récente.
pub fn examine_kept(agent: &Agent, state: &StackState, version: &Version) -> Result<Kept> {
    let app = state.app.as_str();
    let stack = agent.paths.stack(app);
    let manifest = release_manifest(&stack, app, version)?;
    ensure_env(&stack)?;
    let project = release_project(agent, app, version);
    // Tel qu'il est écrit d'abord, comme à l'étape 5 : une version installée par un agent plus
    // ancien n'est pas passée par ce contrôle.
    let compose_file = StackPaths::compose_file(&project.dir);
    let written = fs::read(&compose_file)
        .with_context(|| format!("lecture de {}", compose_file.display()))?;
    compose::check_archived(&written).map_err(|error| KeptRefused {
        version: version.clone(),
        reason: format!("{error} ({})", error.code()),
        with_env: false,
    })?;
    let config_json = agent.docker.config(&project.project())?;
    let report =
        analyze_compose(state, &manifest, &project.dir, &config_json).map_err(|error| {
            KeptRefused {
                version: version.clone(),
                reason: error.to_string(),
                with_env: true,
            }
        })?;
    let new_permissions = state
        .new_permissions(&report)
        .into_iter()
        .cloned()
        .collect();
    Ok(Kept {
        manifest,
        new_permissions,
    })
}

/// Relance une version gardée, déjà contrôlée ([`examine_kept`]), et attend son état sain, avec
/// son propre `health_timeout`. Une fois saine, c'est elle qui tourne : le dossier de contrôle et
/// l'unité des demandes la suivent.
pub fn relaunch(
    agent: &Agent,
    app: &str,
    version: &Version,
    manifest: &StackManifest,
) -> Result<()> {
    let stack = agent.paths.stack(app);
    let dir = stack.release(version);
    ensure_env(&stack)?;
    // Une version installée avant le dossier de contrôle le reçoit à sa relance.
    control::write_compose(&stack, &dir, manifest)?;
    agent.docker.up(
        &Project {
            name: app,
            dir: &dir,
            env_file: &stack.env_file(),
        },
        manifest.health_timeout,
    )?;
    settle_control(agent, app, manifest.app_control.is_some());
    Ok(())
}

/// La version qui tourne : celle restée en échec, dont les conteneurs sont en l'état, sinon
/// l'installée.
fn running_version(state: &StackState) -> Option<&Version> {
    state
        .failed
        .as_ref()
        .or(state.installed.as_ref())
        .map(|record| &record.version)
}

/// La version qui tourne a-t-elle un dossier de contrôle (`app_control` de son `stack.json`) ?
fn running_has_control(stack: &StackPaths, state: &StackState) -> Result<bool> {
    match running_version(state) {
        Some(version) => Ok(release_manifest(stack, &state.app, version)?
            .app_control
            .is_some()),
        None => Ok(false),
    }
}

/// Accorde le dossier de contrôle et l'unité des demandes d'une stack à la version qui vient de
/// démarrer, verrou tenu. Sans `app_control`, ses conteneurs ne montent plus le dossier : il est
/// retiré, et l'unité désactivée. Un échec ici ne défait pas ce qui tourne : c'est un
/// avertissement.
fn settle_control(agent: &Agent, app: &str, has_control: bool) {
    if !has_control && let Err(error) = control::remove(&agent.paths.stack(app)) {
        warn(app, format!("attention : {}", describe(&error)));
    }
    if let Err(error) = auto::sync_stack(agent, app, has_control) {
        warn(app, format!("attention : {}", describe(&error)));
    }
}

/// Pour `auto on` : accorde le dossier de contrôle et l'unité des demandes d'une stack installée
/// à la version qui tourne, sous son verrou. Une stack dont on ne sait pas lire la version qui
/// tourne n'a pas d'unité, et garde son dossier. Seul un échec de systemd est rendu.
pub fn settle_installed(agent: &Agent, app: &str) -> Result<()> {
    let stack = agent.paths.stack(app);
    let examined = (|| -> Result<Option<(fs::File, bool)>> {
        let lock = files::lock_wait(&stack.lock_file(), app)?;
        // Désinstallée pendant l'attente du verrou.
        let Some(mut state) = StackState::load(&agent.paths, app)? else {
            return Ok(None);
        };
        mark_interrupted(&agent.paths, &mut state)?;
        Ok(Some((lock, running_has_control(&stack, &state)?)))
    })();
    match examined {
        Ok(None) => Ok(()),
        Ok(Some((_lock, has_control))) => {
            if !has_control && let Err(error) = control::remove(&stack) {
                warn(app, format!("attention : {}", describe(&error)));
            }
            auto::sync_stack(agent, app, has_control)
        }
        Err(error) => {
            warn(
                app,
                format!(
                    "attention : {} ; les demandes de son interface ne seront pas reçues",
                    describe(&error)
                ),
            );
            auto::sync_stack(agent, app, false)
        }
    }
}

/// `stack.json` d'une version gardée sur la machine.
pub fn release_manifest(stack: &StackPaths, app: &str, version: &Version) -> Result<StackManifest> {
    let path = stack.release(version).join(archive::STACK_JSON);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "la version {version} n'est plus sur cette machine ({})",
            path.display()
        )
    })?;
    StackManifest::parse_published(&bytes, app, version)
        .map_err(|error| anyhow!("{} : {error}", path.display()))
}

/// Docker Compose exige le fichier passé à `--env-file` : un `.env` effacé à la main redevient
/// un fichier vide.
pub fn ensure_env(stack: &StackPaths) -> Result<()> {
    let env = stack.env_file();
    if !env.exists() {
        files::replace(&env, b"", 0o600)?;
    }
    Ok(())
}

/// Retire les dossiers des versions qui ne sont plus gardées, puis leurs images qu'aucune version
/// gardée n'utilise.
pub fn cleanup(agent: &Agent, state: &StackState, dropped: &[ReleaseRecord]) {
    let stack = agent.paths.stack(&state.app);
    let kept: Vec<String> = state
        .kept()
        .map(|record| record.version.to_string())
        .collect();
    if let Ok(entries) = fs::read_dir(stack.releases()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !kept.contains(&name) {
                let _ = files::remove_dir(&entry.path());
                let _ = files::remove_file(&entry.path());
            }
        }
    }
    let used = state.kept_images();
    let mut images: Vec<String> = dropped
        .iter()
        .flat_map(|record| record.images.iter())
        .filter(|image| !used.contains(image.as_str()))
        .cloned()
        .collect();
    images.sort();
    images.dedup();
    agent.docker.remove_images(&images);
}

/// `check [<app>]` : étapes 1 à 5 sans rien installer. La version disponible est examinée puis
/// retirée ; le résultat va dans `status.json`, que lit l'app (docs/PROTOCOLE.md).
pub fn check(agent: &Agent, args: CheckArgs) -> Result<Outcome> {
    let apps = targets(agent, args.app.as_ref())?;
    if apps.is_empty() {
        println!("Aucune stack installée.");
        return Ok(Outcome::Done);
    }
    let session = Session::open(agent)?;
    let mut outcome = Outcome::Done;
    for app in &apps {
        let result = (|| -> Result<Outcome> {
            let stack = agent.paths.stack(app);
            let _lock = files::lock(&stack.lock_file(), app)?;
            let mut state = StackState::require(&agent.paths, app)?;
            mark_interrupted(&agent.paths, &mut state)?;
            let available = check_locked(&session, &mut state)?;
            show_available(&state, available.as_ref());
            Ok(failed_reminder(agent, &state))
        })()
        .unwrap_or_else(|error| {
            eprintln!("erreur : {app} : {}", describe(&error));
            Outcome::Failed
        });
        outcome = outcome.worst(result);
    }
    Ok(outcome)
}

/// Vérifie une stack, verrou déjà pris : la version disponible, téléchargée, vérifiée et
/// contrôlée dans un dossier à part, puis retirée. Rien de ce qui tourne ne change.
fn check_locked(session: &Session<'_>, state: &mut StackState) -> Result<Option<Available>> {
    let stack = session.agent.paths.stack(&state.app);
    let available = match offer(session, state)? {
        None => None,
        Some(offer) => {
            // Le dossier examiné et son .env partent quoi qu'il arrive.
            let _examined_files = Pending::new(stack.check_root(), None, stack.check_env());
            let examined = examine(
                session,
                state,
                &offer,
                &stack.check_dir(&offer.version),
                &stack.check_env(),
            )?;
            let new_permissions = state
                .new_permissions(&examined.report)
                .into_iter()
                .map(ToString::to_string)
                .collect();
            Some(Available {
                version: offer.version.clone(),
                channel: offer.channel,
                pub_date: offer.pub_date.clone(),
                // Lues au terminal et par l'interface de l'app, dans `status.json`.
                notes: visible_lines(
                    &offer
                        .notes
                        .as_deref()
                        .unwrap_or_default()
                        .replace("\r\n", "\n"),
                ),
                new_permissions,
            })
        }
    };
    state.available = available.clone();
    state.checked_at = Some(state::now());
    state.save(&session.agent.paths)?;
    Ok(available)
}

fn show_available(state: &StackState, available: Option<&Available>) {
    let app = &state.app;
    let Some(available) = available else {
        match state.installed_version() {
            Some(installed) => say(app, format!("à jour ({installed})")),
            None => say(app, "aucune version disponible"),
        }
        return;
    };
    match state.installed_version() {
        Some(installed) => say(
            app,
            format!(
                "version {} disponible ({}), installée : {installed}",
                available.version, available.channel
            ),
        ),
        None => say(
            app,
            format!(
                "version {} disponible ({})",
                available.version, available.channel
            ),
        ),
    }
    if !available.notes.trim().is_empty() {
        println!("Notes de version :");
        for line in available.notes.lines() {
            println!("  {}", visible(line));
        }
    }
    if available.new_permissions.is_empty() {
        println!(
            "Pour l'installer : depuis l'interface de l'app, ou sudo drakbase-stack update {app}"
        );
    } else {
        println!("Elle demande de nouvelles permissions :");
        for permission in &available.new_permissions {
            list_item(permission);
        }
        println!("Pour les accepter et l'installer : sudo drakbase-stack update {app} --accept");
    }
}

/// `request <app>` : traite les demandes que l'app a déposées dans `control/request/`, lancé
/// par l'unité systemd `drakbase-stack-request@<app>.path` (docs/PROTOCOLE.md, « Mises à jour
/// validées dans l'app »). Attend le verrou si une autre opération tourne. Une stack dont la
/// version qui tourne n'a pas `app_control` ne dépose pas de demande : c'est un refus, sans que
/// le dossier de contrôle soit lu ni créé.
///
/// L'unité relance cette commande tant que `request/` n'est pas vide. Un arrêt en erreur ne doit
/// donc pas y laisser de demande : elles sont retirées, et l'échec noté pour l'app ; quand même
/// l'état de la stack ne se lit pas, l'unité est désactivée.
pub fn request(agent: &Agent, app: &str) -> Result<Outcome> {
    let stack = agent.paths.stack(app);
    let opened = files::lock_wait(&stack.lock_file(), app)
        .and_then(|lock| Ok((lock, StackState::require(&agent.paths, app)?)));
    let (_lock, mut state) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            // Ni verrou, ni état : rien ne peut être noté. L'unité s'arrête de relancer.
            stop_request_unit(agent, app);
            return Err(error);
        }
    };
    let has_control = mark_interrupted(&agent.paths, &mut state)
        .and_then(|()| running_has_control(&stack, &state));
    let received = match has_control {
        Ok(true) => control::take_requests(&stack),
        Ok(false) => {
            // Une unité des demandes restée activée (un agent plus ancien en donnait une à toute
            // stack) relancerait cette commande tant que le dossier des demandes n'est pas vide.
            stop_request_unit(agent, app);
            bail!(
                "{app} ne reçoit pas de demande de son interface : la version qui tourne n'a pas de dossier de contrôle (app_control dans stack.json). Pour la mettre à jour : sudo drakbase-stack update {app}"
            );
        }
        Err(error) => Err(error),
    };
    let received = match received {
        Ok(received) => received,
        Err(error) => {
            // Les demandes n'ont pas été prises : elles partent sans être lues, et l'app apprend
            // pourquoi. Si même cela échoue, l'unité s'arrête de relancer.
            let noted = control::discard_requests(&stack).and_then(|()| {
                state.operation = Some(finished(
                    Action::Check,
                    None,
                    state::now(),
                    OperationState::Failed,
                    describe(&error),
                ));
                state.save(&agent.paths)
            });
            if noted.is_err() {
                stop_request_unit(agent, app);
            }
            return Err(error);
        }
    };
    if received.is_empty() {
        return Ok(Outcome::Done);
    }
    let session = match Session::open(agent) {
        Ok(session) => session,
        Err(error) => {
            let started = state::now();
            state.operation = Some(finished(
                Action::Check,
                None,
                started,
                OperationState::Failed,
                describe(&error),
            ));
            state.save(&agent.paths)?;
            return Err(error);
        }
    };
    let mut outcome = Outcome::Done;
    for control::Received { name, request } in received {
        let started = state::now();
        let result = match request {
            Err(error) => {
                warn(app, format!("demande « {name} » refusée : {error}"));
                state.operation = Some(finished(
                    Action::Check,
                    None,
                    started,
                    OperationState::Failed,
                    format!("demande refusée : {error}"),
                ));
                Outcome::Failed
            }
            Ok(Request::Check) => request_check(&session, &mut state, started)?,
            Ok(Request::Update { version }) => {
                request_update(&session, &mut state, &version, started)?
            }
        };
        state.save(&agent.paths)?;
        outcome = outcome.worst(result);
    }
    Ok(outcome)
}

/// Désactive l'unité des demandes d'une stack, pour qu'elle ne relance plus `request`.
fn stop_request_unit(agent: &Agent, app: &str) {
    if let Err(error) = auto::sync_stack(agent, app, false) {
        warn(app, format!("attention : {}", describe(&error)));
    }
}

/// Une opération encore `running` quand l'agent tient le verrou a été laissée par un processus
/// arrêté au milieu : elle devient `failed`, pour que l'app ne l'attende plus. Toute commande qui
/// prend le verrou d'une stack installée l'appelle.
pub fn mark_interrupted(paths: &Paths, state: &mut StackState) -> Result<()> {
    let Some(operation) = &mut state.operation else {
        return Ok(());
    };
    if operation.state != OperationState::Running {
        return Ok(());
    }
    operation.state = OperationState::Failed;
    operation.finished_at = Some(state::now());
    operation.message = format!("opération interrompue ({})", operation.message);
    state.save(paths)
}

fn finished(
    action: Action,
    version: Option<Version>,
    started_at: String,
    state: OperationState,
    message: String,
) -> Operation {
    Operation {
        action,
        version,
        state,
        started_at,
        finished_at: Some(state::now()),
        message,
    }
}

fn running(
    session: &Session<'_>,
    state: &mut StackState,
    action: Action,
    version: Option<Version>,
    started_at: &str,
    message: String,
) -> Result<()> {
    state.operation = Some(Operation {
        action,
        version,
        state: OperationState::Running,
        started_at: started_at.to_owned(),
        finished_at: None,
        message,
    });
    state.save(&session.agent.paths)
}

fn request_check(
    session: &Session<'_>,
    state: &mut StackState,
    started: String,
) -> Result<Outcome> {
    let app = state.app.clone();
    say(&app, "vérification demandée par l'interface");
    running(
        session,
        state,
        Action::Check,
        None,
        &started,
        "vérification en cours".into(),
    )?;
    let (operation_state, message, outcome) = match check_locked(session, state) {
        Ok(None) => (
            OperationState::Done,
            match state.installed_version() {
                Some(installed) => format!("à jour ({installed})"),
                None => "aucune version disponible".to_owned(),
            },
            Outcome::Done,
        ),
        Ok(Some(available)) if available.new_permissions.is_empty() => (
            OperationState::Done,
            format!("version {} disponible", available.version),
            Outcome::Done,
        ),
        Ok(Some(available)) => (
            OperationState::Done,
            format!(
                "version {} disponible ; elle demande de nouvelles permissions, {}",
                available.version,
                accept_hint(&app)
            ),
            Outcome::Done,
        ),
        Err(error) => {
            warn(&app, describe(&error));
            (OperationState::Failed, describe(&error), Outcome::Failed)
        }
    };
    if let Some(available) = &state.available {
        show_available(state, Some(&available.clone()));
    }
    state.operation = Some(finished(
        Action::Check,
        None,
        started,
        operation_state,
        message,
    ));
    Ok(outcome)
}

fn request_update(
    session: &Session<'_>,
    state: &mut StackState,
    version: &Version,
    started: String,
) -> Result<Outcome> {
    let app = state.app.clone();
    let app = app.as_str();
    say(
        app,
        format!("installation de la version {version} demandée par l'interface"),
    );
    let refusal = match &state.available {
        Some(available)
            if &available.version == version && available.new_permissions.is_empty() =>
        {
            None
        }
        Some(available) if &available.version == version => Some((
            OperationState::Permissions,
            format!(
                "la version {version} demande de nouvelles permissions, {}",
                accept_hint(app)
            ),
        )),
        _ => Some((
            OperationState::Failed,
            format!(
                "la version {version} n'est pas celle que propose la dernière vérification : vérifiez de nouveau"
            ),
        )),
    };
    if let Some((operation_state, message)) = refusal {
        warn(app, &message);
        let outcome = if operation_state == OperationState::Permissions {
            Outcome::PermissionRequired
        } else {
            Outcome::Failed
        };
        state.operation = Some(finished(
            Action::Update,
            Some(version.clone()),
            started,
            operation_state,
            message,
        ));
        return Ok(outcome);
    }

    running(
        session,
        state,
        Action::Update,
        Some(version.clone()),
        &started,
        format!("installation de la version {version}"),
    )?;
    // L'utilisateur redemande cette version : une version écartée est reprise, comme avec
    // --retry. Les permissions, elles, ne s'acceptent que sur la machine.
    let options = Options {
        accept: false,
        retry: true,
        install: false,
    };
    let previous = state.installed_version().cloned();
    // La vérification est refaite, et seule la version demandée s'installe : c'est celle dont
    // l'utilisateur a lu les notes. Si le serveur en propose une autre entre-temps, ou plus
    // aucune, rien n'est téléchargé.
    let result = match offer(session, state) {
        Ok(Some(offered)) if &offered.version == version => {
            apply_offer(session, state, &options, &offered)
        }
        Ok(offered) => {
            let message = match offered {
                Some(offered) => format!(
                    "le serveur propose maintenant la version {}, pas la {version} : rien n'a été installé, vérifiez de nouveau",
                    offered.version
                ),
                None => format!(
                    "la version {version} n'est plus proposée par le serveur : rien n'a été installé, vérifiez de nouveau"
                ),
            };
            warn(app, &message);
            state.operation = Some(finished(
                Action::Update,
                Some(version.clone()),
                started,
                OperationState::Failed,
                message,
            ));
            return Ok(Outcome::Failed);
        }
        Err(error) => Err(error),
    };
    let (operation_state, message, outcome) = match result.map(|applied| {
        let Applied {
            outcome,
            blocked_return,
        } = applied;
        (outcome, blocked_return)
    }) {
        // La version a échoué, et la précédente n'a pas été relancée : l'opération a échoué, et
        // son message dit la commande à taper.
        Ok((outcome, Some(blocked_return))) => (OperationState::Failed, blocked_return, outcome),
        Ok((Outcome::Done, None)) => (
            OperationState::Done,
            format!("version {version} en place"),
            Outcome::Done,
        ),
        Ok((Outcome::PermissionRequired, None)) => (
            OperationState::Permissions,
            format!(
                "la version {version} demande de nouvelles permissions, {}",
                accept_hint(app)
            ),
            Outcome::PermissionRequired,
        ),
        Ok((Outcome::Failed, None)) => {
            let message = if state
                .failed
                .as_ref()
                .is_some_and(|failed| &failed.version == version)
            {
                match &previous {
                    Some(previous) => format!(
                        "la version {version} n'est pas devenue saine ; ses conteneurs sont restés en l'état. Pour revenir à la {previous} : sudo drakbase-stack rollback {app}"
                    ),
                    None => format!(
                        "la version {version} n'est pas devenue saine ; ses conteneurs sont restés en l'état"
                    ),
                }
            } else {
                match state.installed_version() {
                    Some(installed) => format!(
                        "la version {version} n'est pas devenue saine : la {installed} a été relancée"
                    ),
                    None => format!("la version {version} n'est pas devenue saine"),
                }
            };
            (OperationState::Failed, message, Outcome::Failed)
        }
        Err(error) => {
            warn(app, describe(&error));
            let mut message = describe(&error);
            // Un échec de `pull` ou de préparation le dit déjà.
            if let Some(in_place) =
                in_place(state).filter(|in_place| !message.contains(in_place.as_str()))
            {
                message.push_str(&format!(" ; {in_place}"));
            }
            (OperationState::Failed, message, Outcome::Failed)
        }
    };
    state.operation = Some(finished(
        Action::Update,
        Some(version.clone()),
        started,
        operation_state,
        message,
    ));
    Ok(outcome)
}

fn up_to_date(session: &Session<'_>, state: &StackState, options: &Options) -> Result<Outcome> {
    let app = &state.app;
    if options.install {
        bail!(
            "aucune version de {app} n'est publiée pour {} sur le canal {} (ou elle demande une version de Docker Compose plus récente que {})",
            session.arch,
            state.channel,
            session.versions.compose
        );
    }
    match state.installed_version() {
        Some(installed) => say(app, format!("à jour ({installed})")),
        None => say(app, "aucune version disponible"),
    }
    Ok(failed_reminder(session.agent, state))
}

/// Une stack dont la dernière version n'est pas devenue saine n'est pas « à jour ».
fn failed_reminder(agent: &Agent, state: &StackState) -> Outcome {
    match &state.failed {
        Some(failed) => {
            warn(
                &state.app,
                format!(
                    "la version {} n'est pas devenue saine et ses conteneurs sont restés en l'état : {}",
                    failed.version,
                    failed_way_out(agent, state, &failed.version)
                ),
            );
            Outcome::Failed
        }
        None => Outcome::Done,
    }
}

/// Comment sortir d'une version restée en échec, verrou tenu. La version installée est contrôlée
/// pour ce seul conseil, comme après l'échec : `rollback` n'est conseillé que s'il peut la
/// relancer, avec `--accept` s'il le faut. Un contrôle impossible (Docker arrêté) laisse le conseil
/// habituel, que `rollback` corrigera.
pub fn failed_way_out(agent: &Agent, state: &StackState, failed: &Version) -> String {
    let app = state.app.as_str();
    let retry = format!("sudo drakbase-stack update {app} --retry pour la reprendre");
    let Some(installed) = state.installed_version() else {
        return format!("{retry}, ou sudo drakbase-stack uninstall {app} pour tout retirer");
    };
    let usual = format!(
        "sudo drakbase-stack rollback {app} pour revenir à la version {installed}, ou {retry}"
    );
    // Un rappel ne crée rien : sans `.env`, que le contrôle écrirait, le conseil habituel suffit.
    if !agent.paths.stack(app).env_file().exists() {
        return usual;
    }
    let (flag, accept) = match examine_kept(agent, state, installed) {
        Ok(kept) if kept.new_permissions.is_empty() => ("", ""),
        Ok(_) => (" --accept", ", en acceptant ses nouvelles permissions"),
        Err(error) => match kept_refused(&error) {
            Some(with_env) => {
                return format!(
                    "la version {installed} ne peut plus être relancée, son compose n'est plus admis. {}",
                    refused_way_out(
                        &agent.paths.stack(app),
                        app,
                        Some(failed),
                        with_env,
                        "rollback"
                    )
                );
            }
            None => ("", ""),
        },
    };
    format!(
        "sudo drakbase-stack rollback {app}{flag} pour revenir à la version {installed}{accept}, ou {retry}"
    )
}

/// Montre les permissions nouvelles et les fait accepter : question à l'installation en mode
/// interactif, `--accept` sinon. Une mise à jour ne pose jamais la question : elle s'arrête.
fn accept_permissions(
    session: &Session<'_>,
    state: &mut StackState,
    permissions: &[&Permission],
    options: &Options,
    version: &Version,
) -> Result<bool> {
    let app = state.app.clone();
    if options.install {
        println!("{app} demande à cette machine :");
    } else {
        println!("{app} : la version {version} demande de nouvelles permissions :");
    }
    for permission in permissions {
        list_item(permission);
    }
    let accepted = if options.accept {
        println!("Permissions acceptées (--accept).");
        true
    } else if options.install && session.agent.interactive {
        let yes = prompt::confirm("Accepter ces permissions ?").context("lecture de la réponse")?;
        if !yes {
            println!("Installation abandonnée : rien n'a été démarré.");
        }
        yes
    } else if options.install {
        println!("Pour les accepter, relancez la même commande avec --accept.");
        false
    } else {
        println!("Pour les accepter : sudo drakbase-stack update {app} --accept");
        false
    };
    if accepted {
        state.accept(permissions.iter().map(|permission| permission.key()));
        if !options.install {
            state.save(&session.agent.paths)?;
        }
    }
    Ok(accepted)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    Ok(read_optional_bytes(path)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// Le contenu d'un fichier, octet pour octet, ou `None` s'il n'existe pas.
fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("lecture de {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_that_reaches_an_engine_socket_through_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("vrai");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("docker.sock"), "").unwrap();
        std::os::unix::fs::symlink(real.join("docker.sock"), dir.path().join("lien")).unwrap();
        let config = |source: &Path| {
            serde_json::to_vec(&serde_json::json!({
                "services": { "app": { "volumes": [
                    { "type": "bind", "source": source, "target": "/x" },
                ] } },
            }))
            .unwrap()
        };
        let error = check_links(&config(&dir.path().join("lien")), "/srv/projet")
            .unwrap_err()
            .to_string();
        assert!(error.contains("par un lien de la machine"), "{error}");
        assert!(
            error.contains("la socket d'un moteur de conteneurs"),
            "{error}"
        );
        // Sous son vrai chemin, sans lien, ou absente (Docker la créera) : rien à redire.
        for source in [
            real.join("docker.sock"),
            real.clone(),
            dir.path().join("absent"),
        ] {
            check_links(&config(&source), "/srv/projet").unwrap();
        }
    }

    #[test]
    fn a_writable_source_that_reaches_a_system_folder_through_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // `/usr` est un vrai dossier sous Linux comme sous macOS.
        let link = dir.path().join("donnees");
        std::os::unix::fs::symlink("/usr", &link).unwrap();
        let config = |read_only: bool| {
            serde_json::to_vec(&serde_json::json!({
                "services": { "app": { "volumes": [
                    { "type": "bind", "source": link, "target": "/x", "read_only": read_only },
                ] } },
            }))
            .unwrap()
        };
        let error = check_links(&config(false), "/srv/projet")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("par un lien de la machine, à /usr"),
            "{error}"
        );
        assert!(error.contains("un dossier du système"), "{error}");
        // En lecture seule, ou sous son vrai chemin : la permission dit ce qu'il en est.
        check_links(&config(true), "/srv/projet").unwrap();
        let direct = serde_json::to_vec(&serde_json::json!({
            "services": { "app": { "volumes": [
                { "type": "bind", "source": "/usr/share", "target": "/x" },
            ] } },
        }))
        .unwrap();
        check_links(&direct, "/srv/projet").unwrap();
    }
}
