//! `drakbase doctor [<app>]` : contrôle tout ce dont les mises à jour d'une app ont besoin, dans
//! le projet du dossier courant, et dit quoi faire pour chaque problème. Rien n'est modifié.
//!
//! Connexion de l'outil, clés de l'app et leur sauvegarde, configuration du projet (Tauri, Swift,
//! stack Docker ou app cli), code qui branche l'updater, app et clé sur le serveur, version à
//! publier, certificat de signature de code, et scripts que le bash de macOS lirait de travers.
//! Pour une app Tauri, chaque réglage est lu comme le lira `tauri build`, OS par OS : un fichier
//! de plateforme (`tauri.macos.conf.json`…) peut le changer pour ses builds (`tauri.rs`).
//! Pour une stack : `stack.json`, Docker, buildx et Docker Compose, et le compose, lu par Docker
//! Compose et contrôlé comme à la publication. La version du projet se compare comme le fera
//! `publish` : à la dernière bêta pour une pré-publication, à la dernière stable sinon. Le code de
//! sortie vaut 1 s'il reste un problème.
//! `init` reprend ces contrôles à sa fin, en ne montrant que ce qui ne va pas.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use am_protocol::signature::KeyId;
use am_protocol::{Channel, Os, Version};
use anyhow::{Context, Result};
use serde_json::Value;

use crate::api::{self, AppKind, KeyStatus};
use crate::backup;
use crate::cli_project::{CliManifest, CliProject};
use crate::connection::{
    self, ConnectError, Connection, LISTING_SCOPE, OTHER_APP_ADVICE, PUBLISH_SCOPE, WRITE_SCOPE,
};
use crate::credentials::{Secrets, ServerUrl};
use crate::home::{AppSlug, Home, HomeArgs, KeyRole};
use crate::pack;
use crate::project::{
    self, HelperRange, Hints, Json, Project, RangeFit, SwiftManifest, SwiftProject, TauriProject,
};
use crate::stack::docker::Tools;
use crate::stack::{self, StackProject};
use crate::tauri::{self, TauriConfig};

/// Pour les essais seulement : la liste des certificats de signature de code, une ligne par nom,
/// à la place de celle du trousseau.
const IDENTITIES_VAR: &str = "APPSMARKET_CODESIGN_IDENTITIES";
/// Ce qu'il faut faire d'un réglage qu'un fichier de plateforme de Tauri donne mal.
const PLATFORM_FIX: &str = "corrige ce fichier de plateforme, ou retires-en ce réglage : drakbase init ne modifie que tauri.conf.json";

#[derive(clap::Args)]
pub struct Args {
    /// Identifiant de l'app (slug) [défaut : celui de l'adresse de vérification du projet]
    #[arg(value_name = "APP")]
    app: Option<AppSlug>,

    /// Info.plist de l'app Swift [défaut : le seul Info.plist du projet]
    #[arg(long, value_name = "FICHIER")]
    plist: Option<PathBuf>,

    /// Configuration de l'app Tauri, tauri.conf.json ou son dossier [défaut :
    /// src-tauri/tauri.conf.json ou tauri.conf.json du dossier courant]
    #[arg(long = "tauri-config", value_name = "CHEMIN")]
    tauri_config: Option<PathBuf>,

    /// Bundle macOS construit, dont contrôler aussi la signature de code et l'Info.plist
    #[arg(long, value_name = "NOM.app")]
    bundle: Option<PathBuf>,

    /// Une stack Docker : son dossier, avec compose.yaml et stack.json [défaut : stack]
    #[arg(
        long,
        value_name = "DOSSIER",
        num_args = 0..=1,
        default_missing_value = stack::DEFAULT_DIR,
        conflicts_with_all = ["plist", "tauri_config", "bundle", "cli"]
    )]
    stack: Option<PathBuf>,

    /// Une app cli (exécutable Linux), dont drakbase.json est à la racine du projet
    #[arg(long, conflicts_with_all = ["plist", "tauri_config", "bundle"])]
    cli: bool,

    #[command(flatten)]
    home: HomeArgs,
}

/// Ce qu'il faut contrôler : les options de `doctor`, que reprend `init`.
pub struct Options {
    pub app: Option<AppSlug>,
    pub plist: Option<PathBuf>,
    pub tauri_config: Option<PathBuf>,
    pub bundle: Option<PathBuf>,
    pub stack: Option<PathBuf>,
    pub cli: bool,
}

/// Le compte rendu : une ligne par contrôle, et le décompte des problèmes.
#[derive(Default)]
struct Report {
    problems: usize,
    warnings: usize,
    /// Seuls les avertissements et les problèmes s'affichent.
    quiet: bool,
}

impl Report {
    fn section(&self, title: &str) {
        if !self.quiet {
            println!();
            println!("{title}");
        }
    }

    fn ok(&mut self, text: impl AsRef<str>) {
        if !self.quiet {
            println!("✓ {}", text.as_ref());
        }
    }

    /// Une information, qui n'est ni un succès ni un problème.
    fn note(&mut self, text: impl AsRef<str>) {
        if !self.quiet {
            println!("· {}", text.as_ref());
        }
    }

    fn warn(&mut self, text: impl AsRef<str>, advice: Option<&str>) {
        self.warnings += 1;
        println!("! {}", text.as_ref());
        if let Some(advice) = advice {
            println!("  → {advice}");
        }
    }

    fn fail(&mut self, text: impl AsRef<str>, advice: Option<&str>) {
        self.problems += 1;
        println!("✗ {}", text.as_ref());
        if let Some(advice) = advice {
            println!("  → {advice}");
        }
    }

    fn summary(&self) {
        match (self.problems, self.warnings) {
            (0, 0) => println!("Tout est prêt."),
            (0, warnings) => println!("Prêt, avec {warnings} avertissement(s)."),
            (problems, warnings) => {
                println!("{problems} problème(s) à régler, {warnings} avertissement(s).")
            }
        }
    }
}

/// Ce que le projet dit de l'app, pour les contrôles du serveur et de la signature.
#[derive(Default)]
struct Facts {
    kind: Option<AppKind>,
    bundle_id: Option<String>,
    version: Option<Version>,
    /// `bundle.macOS.signingIdentity` d'une app Tauri, tel que le lisent ses builds macOS.
    signing_identity: Option<String>,
    /// Le fichier qui le donne : `tauri.conf.json`, ou `tauri.macos.conf.json` s'il le change.
    signing_source: Option<String>,
    /// Ce fichier est celui de la plateforme, qu'`init --sign-id` ne modifie pas.
    signing_in_platform_file: bool,
    /// L'app que désigne l'adresse de vérification du projet.
    configured_app: Option<AppSlug>,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let mut report = Report::default();
    println!("drakbase {}", project::CLIENT_VERSION);
    examine(
        &home,
        &Options {
            app: args.app,
            plist: args.plist,
            tauri_config: args.tauri_config,
            bundle: args.bundle,
            stack: args.stack,
            cli: args.cli,
        },
        &mut report,
    )?;
    println!();
    report.summary();
    Ok(if report.problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Les mêmes contrôles, pour la fin d'`init` : seuls les avertissements et les problèmes
/// s'affichent, puis le décompte.
pub fn check_quietly(home: &Home, options: &Options) -> Result<()> {
    let mut report = Report {
        quiet: true,
        ..Report::default()
    };
    examine(home, options, &mut report)?;
    report.summary();
    Ok(())
}

fn examine(home: &Home, args: &Options, report: &mut Report) -> Result<()> {
    report.section("Connexion");
    let secrets = Secrets::select(home)?;
    let session = connection(&secrets, report);
    let server = session
        .as_ref()
        .map(|connection| connection.server().as_str().to_owned());

    report.section("Projet");
    let hints = Hints {
        tauri_config: args.tauri_config.as_deref(),
        plist: args.plist.as_deref(),
        stack: args.stack.as_deref(),
        cli: args.cli,
    };
    let project = match project::detect(Path::new(""), &hints) {
        Ok(Some(project)) => {
            match &project {
                Project::Tauri(tauri) => {
                    report.ok(format!("app Tauri : {}", project::display(&tauri.config)));
                }
                Project::Swift(swift) => {
                    let manifest = match &swift.manifest {
                        SwiftManifest::Package(path) | SwiftManifest::Xcode(path) => {
                            project::display(path)
                        }
                    };
                    report.ok(format!("app Swift : {manifest}"));
                }
                Project::Stack(stack) => {
                    report.ok(format!("stack Docker : {}", project::display(&stack.dir)));
                }
                Project::Cli(cli) => {
                    report.ok(format!("app cli : {}", project::display(&cli.file)));
                }
            }
            Some(project)
        }
        Ok(None) => {
            report.warn(
                "aucun projet d'app dans le dossier courant : ni tauri.conf.json, ni Package.swift, ni projet Xcode, ni stack/compose.yaml, ni drakbase.json",
                Some("lance doctor à la racine du projet de l'app, ou donne --tauri-config, --stack <dossier> ou --cli"),
            );
            None
        }
        Err(error) => {
            report.fail(format!("{error:#}"), None);
            None
        }
    };

    let mut facts = project.as_ref().map(configured).unwrap_or_default();
    let app = args.app.clone().or(facts.configured_app.clone());
    match &app {
        Some(app) => report.ok(format!("app : {app}")),
        None => report.fail(
            "impossible de savoir quelle app contrôler",
            Some("donne son identifiant (drakbase doctor <app>), ou branche le projet avec drakbase init <app>"),
        ),
    }

    if let Some(app) = &app {
        report.section("Clés");
        let main_id = keys(home, &secrets, app, report);

        if let Some(project) = &project {
            report.section("Configuration du projet");
            match project {
                Project::Tauri(tauri) => {
                    check_tauri(tauri, app, server.as_deref(), main_id, report);
                }
                Project::Swift(swift) => {
                    check_swift(swift, app, server.as_deref(), main_id, report);
                }
                Project::Stack(stack) => {
                    let host = session
                        .as_ref()
                        .map(|connection| connection.server().image_host());
                    check_stack(stack, app, host, main_id, report);
                }
                Project::Cli(cli) => check_cli(cli, app, main_id, report),
            }
        }

        if let Some(bundle) = &args.bundle {
            report.section("Bundle");
            check_bundle(bundle, app, server.as_deref(), main_id, &mut facts, report);
        }

        if let Some(connection) = &session {
            report.section("Serveur");
            check_server(connection, app, &facts, main_id, report);
        }
    }

    if matches!(facts.kind, Some(AppKind::Swift | AppKind::Tauri)) || args.bundle.is_some() {
        report.section("Signature de code (macOS)");
        check_codesign(&facts, report);
    }

    if let Some(project) = &project {
        let root = match project {
            Project::Tauri(tauri) => tauri.root.clone(),
            Project::Swift(swift) => swift.root.clone(),
            Project::Stack(stack) => stack
                .dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            Project::Cli(cli) => cli.root.clone(),
        };
        report.section("Scripts");
        check_scripts(&root, report);
    }

    Ok(())
}

/// La connexion enregistrée, vérifiée auprès du serveur ; `None`, et un problème au compte rendu,
/// si elle manque ou si le serveur la refuse.
fn connection(secrets: &Secrets, report: &mut Report) -> Option<Connection> {
    let connection = match Connection::open(secrets) {
        Ok(connection) => connection,
        Err(ConnectError::NotLoggedIn { place }) => {
            report.fail(
                format!("aucune connexion enregistrée dans {place}"),
                Some("connecte l'outil : drakbase login --server https://…, avec un jeton créé dans l'administration"),
            );
            return None;
        }
        Err(ConnectError::Unreadable(error)) => {
            report.fail(format!("connexion illisible : {error:#}"), None);
            return None;
        }
        Err(ConnectError::Refused { server, error }) => {
            report.fail(
                format!("{server} refuse ou ne répond pas : {error}"),
                Some("si le jeton a expiré ou a été révoqué, crée-en un autre puis relance drakbase login"),
            );
            return None;
        }
    };
    report.ok(connection.describe());
    let token = &connection.whoami.token;
    if !token.has_scope(WRITE_SCOPE) {
        report.fail(
            "le jeton ne peut pas envoyer de version (portée releases:write absente)",
            Some("crée un jeton avec cette portée, puis relance drakbase login"),
        );
    }
    if !token.has_scope(PUBLISH_SCOPE) {
        report.warn(
            "le jeton ne peut pas publier (portée releases:publish absente) : les versions resteront en brouillon",
            Some("publie-les depuis l'administration, ou crée un jeton avec cette portée"),
        );
    }
    if !token.has_scope(LISTING_SCOPE) {
        report.warn(
            "le jeton ne peut pas modifier la fiche publique de l'app (portée listing:write absente) : drakbase listing ne changera ni sa description ni ses captures d'écran",
            Some("change-les dans l'administration, ou crée un jeton avec cette portée puis relance drakbase login"),
        );
    }
    if let Some(warning) = connection::expiry_warning(token) {
        report.warn(warning, None);
    }
    Some(connection)
}

/// Ce que la configuration du projet dit de l'app, avant tout contrôle.
fn configured(project: &Project) -> Facts {
    match project {
        Project::Tauri(project) => {
            let config = TauriConfig::read(&project.config).ok();
            // L'identifiant, la version et le certificat sont ceux des builds macOS : le serveur
            // ne contrôle que l'identifiant des archives macOS.
            let text = |pointer: &str| -> Option<String> {
                let setting = config.as_ref()?.get(Os::Darwin, pointer);
                setting.as_str().map(str::to_owned)
            };
            let configured_app = config.as_ref().and_then(|config| {
                tauri::OSES.iter().find_map(|os| {
                    endpoints(config.get(*os, tauri::ENDPOINTS).value)
                        .iter()
                        .find_map(|url| project::app_of_feed(url))
                })
            });
            let identity = config
                .as_ref()
                .map(|config| config.get(Os::Darwin, tauri::SIGNING_IDENTITY));
            Facts {
                kind: Some(AppKind::Tauri),
                bundle_id: text("/identifier"),
                version: tauri_version(project, text("/version")),
                signing_identity: text(tauri::SIGNING_IDENTITY),
                signing_source: identity.map(|identity| file_name(identity.source)),
                signing_in_platform_file: identity
                    .is_some_and(|identity| identity.source != project.config),
                configured_app,
            }
        }
        Project::Stack(_) => Facts {
            // stack.json ne dit pas l'app : son slug n'y entre qu'à la publication.
            kind: Some(AppKind::Stack),
            ..Facts::default()
        },
        Project::Cli(cli) => Facts {
            kind: Some(AppKind::Cli),
            version: fs::read_to_string(cli.root.join("Cargo.toml"))
                .ok()
                .and_then(|text| project::cargo_package_version(&text)),
            configured_app: CliManifest::read(&cli.file)
                .ok()
                .and_then(|manifest| manifest.app)
                .and_then(|app| app.parse().ok()),
            ..Facts::default()
        },
        Project::Swift(swift) => {
            let info = swift.plist.as_deref().and_then(read_plist);
            let text = |key: &str| -> Option<String> {
                info.as_ref()?.get(key)?.as_string().map(str::to_owned)
            };
            let version = text("CFBundleShortVersionString")
                .and_then(|text| project::literal_version(&text))
                .or_else(|| {
                    fs::read_to_string(swift.root.join("VERSION"))
                        .ok()
                        .and_then(|text| project::literal_version(&text))
                });
            Facts {
                kind: Some(AppKind::Swift),
                bundle_id: text("CFBundleIdentifier").filter(|text| !text.contains("$(")),
                version,
                configured_app: text(project::FEED_KEY.name)
                    .or_else(|| text(project::FEED_KEY.legacy))
                    .and_then(|url| project::app_of_feed(&url)),
                ..Facts::default()
            }
        }
    }
}

/// La version d'une app Tauri, comme la lit `tauri build` : écrite dans tauri.conf.json, ou prise
/// dans le package.json qu'elle désigne (`"version": "../package.json"`), sinon celle du
/// Cargo.toml.
fn tauri_version(project: &TauriProject, written: Option<String>) -> Option<Version> {
    match written {
        Some(text) if text.ends_with(".json") => Json::read(&project.src_tauri.join(text))
            .ok()?
            .get(&["version"])?
            .as_str()
            .and_then(project::literal_version),
        Some(text) => project::literal_version(&text),
        None => fs::read_to_string(project.src_tauri.join("Cargo.toml"))
            .ok()
            .and_then(|text| project::cargo_package_version(&text)),
    }
}

fn read_plist(path: &Path) -> Option<plist::Dictionary> {
    plist::Value::from_file(path).ok()?.into_dictionary()
}

/// Les adresses de vérification d'un réglage `plugins.updater.endpoints`.
fn endpoints(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Le nom d'un fichier, sans son dossier : `tauri.macos.conf.json`.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || project::display(path),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Un réglage d'une app Tauri commun aux trois OS, pour ceux qui le lisent dans le même fichier.
struct Scoped<'a> {
    /// Rien quand tous les OS le lisent dans `tauri.conf.json` ; sinon de qui il s'agit :
    /// «  pour macOS (src-tauri/tauri.macos.conf.json) ».
    scope: String,
    value: Option<&'a Value>,
    /// Il vient du fichier d'une plateforme, qu'`init` ne modifie pas.
    platform_file: bool,
}

impl Scoped<'_> {
    /// Le conseil : celui d'`init` pour `tauri.conf.json`, sinon corriger le fichier de plateforme.
    fn fix<'f>(&self, init: &'f str) -> &'f str {
        if self.platform_file {
            PLATFORM_FIX
        } else {
            init
        }
    }
}

/// Un réglage commun aux trois OS, tel que le lit `tauri build` pour chacun : les OS qui le lisent
/// dans le même fichier sont regroupés. Sans fichier de plateforme qui le change, il n'y a qu'un
/// groupe, celui de `tauri.conf.json`.
fn platform_settings<'a>(config: &'a TauriConfig, pointer: &str) -> Vec<Scoped<'a>> {
    let mut groups: Vec<(Vec<Os>, tauri::Setting<'a>)> = Vec::new();
    for os in tauri::OSES {
        let setting = config.get(os, pointer);
        match groups
            .iter_mut()
            .find(|(_, known)| known.source == setting.source)
        {
            Some((oses, _)) => oses.push(os),
            None => groups.push((vec![os], setting)),
        }
    }
    let single = groups.len() == 1;
    let main = config.path();
    groups
        .into_iter()
        .map(|(oses, setting)| {
            let names: Vec<&str> = oses.iter().map(|os| tauri::os_name(*os)).collect();
            let scope = if single {
                String::new()
            } else {
                format!(
                    " pour {} ({})",
                    names.join(" et "),
                    project::display(setting.source)
                )
            };
            Scoped {
                scope,
                value: setting.value,
                platform_file: setting.source != main,
            }
        })
        .collect()
}

/// Les clés locales de l'app ; rend l'identifiant de la clé principale.
fn keys(home: &Home, secrets: &Secrets, app: &AppSlug, report: &mut Report) -> Option<KeyId> {
    let main = home.key_files(app, KeyRole::Main);
    let backup = home.key_files(app, KeyRole::Backup);
    let main_id = match fs::read_to_string(&main.public).map(|text| project::key_id(&text)) {
        Ok(Ok(id)) => Some(id),
        Ok(Err(_)) => {
            report.fail(format!("{} illisible", main.public.display()), None);
            None
        }
        Err(_) => {
            report.fail(
                format!("pas de clé publique principale ({})", main.public.display()),
                Some(&format!(
                    "crée les clés avec drakbase init {app} (ou keygen), ou restaure-les depuis ta sauvegarde"
                )),
            );
            None
        }
    };
    match fs::metadata(&main.secret) {
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                report.warn(
                    format!(
                        "{} est lisible par d'autres comptes ({mode:o})",
                        main.secret.display()
                    ),
                    Some(&format!("chmod 600 {}", main.secret.display())),
                );
            }
            if let Some(id) = main_id {
                report.ok(format!("clé principale {id} ({})", main.secret.display()));
            }
        }
        Err(_) if main_id.is_some() => report.fail(
            format!(
                "clé secrète principale {} introuvable : aucune version ne pourra être signée",
                main.secret.display()
            ),
            Some("restaure-la depuis ta sauvegarde"),
        ),
        Err(_) => {}
    }
    match fs::read_to_string(&backup.public).map(|text| project::key_id(&text)) {
        Ok(Ok(id)) => report.ok(format!("clé de secours {id}")),
        _ => report.warn(
            format!("pas de clé de secours ({})", backup.public.display()),
            Some("elle sauve les apps Swift, les stacks et les apps cli si la clé principale est perdue"),
        ),
    }
    if let Some(id) = main_id {
        check_backup(home, secrets, app, id, report);
    }
    main_id
}

/// Le mot de passe de la clé principale, rangé, et la sauvegarde des clés, vérifiée.
fn check_backup(
    home: &Home,
    secrets: &Secrets,
    app: &AppSlug,
    main_id: KeyId,
    report: &mut Report,
) {
    match secrets.has_key_password(main_id) {
        Ok(true) => report.ok(format!(
            "mot de passe de la clé principale dans {}",
            secrets.key_passwords_place()
        )),
        Ok(false) => report.warn(
            format!(
                "mot de passe de la clé principale absent {} : chaque signature le demandera",
                secrets.key_passwords_place_after_de()
            ),
            Some(&format!(
                "drakbase backup {app} l'y range, après l'avoir vérifié"
            )),
        ),
        Err(error) => report.warn(
            format!("mot de passe de la clé principale : {error:#}"),
            None,
        ),
    }
    match backup::load(home, app) {
        Ok(Some(record)) if record.main == main_id.to_hex() => report.ok(format!(
            "clés sauvegardées dans KeePassXC ({}), vérifiées le {}",
            record.keepassxc.display(),
            record.day()
        )),
        Ok(Some(record)) => report.fail(
            format!(
                "la sauvegarde notée est celle d'une autre clé principale ({}) : celle-ci n'est sauvegardée nulle part",
                record.main
            ),
            Some(&format!("drakbase backup {app}")),
        ),
        Ok(None) => report.fail(
            "clés jamais sauvegardées hors de ce Mac : si elles se perdent, plus aucune mise à jour ne pourra être signée",
            Some(&format!("drakbase backup {app} les range dans KeePassXC, et vérifie la sauvegarde")),
        ),
        Err(error) => report.fail(format!("{error:#}"), None),
    }
}

/// L'adresse attendue, ou à défaut une adresse qui désigne bien l'app. `fix` : ce qu'il faut faire
/// d'une adresse absente ou fausse.
fn check_feed(
    report: &mut Report,
    found: &[String],
    expected: Option<String>,
    app: &AppSlug,
    what: &str,
    fix: &str,
) {
    match expected {
        Some(expected) if found.contains(&expected) => report.ok(format!("{what} : {expected}")),
        Some(expected) => report.fail(
            format!(
                "{what} : {} au lieu de {expected}",
                if found.is_empty() {
                    "absente".to_owned()
                } else {
                    found.join(", ")
                }
            ),
            Some(fix),
        ),
        None => match found
            .iter()
            .find(|url| project::app_of_feed(url).as_ref() == Some(app))
        {
            Some(url) => report.ok(format!(
                "{what} : {url} (serveur non vérifié, faute de connexion)"
            )),
            None => report.fail(format!("{what} : aucune ne désigne l'app {app}"), Some(fix)),
        },
    }
}

/// La clé embarquée doit être la clé principale de l'app. `fix` : ce qu'il faut faire d'une clé
/// absente.
fn check_embedded(
    report: &mut Report,
    embedded: Option<KeyId>,
    main_id: Option<KeyId>,
    what: &str,
    fix: &str,
) {
    match (embedded, main_id) {
        (None, _) => report.fail(format!("{what} : absente ou illisible"), Some(fix)),
        (Some(id), Some(main)) if id == main => report.ok(format!("{what} : {id}, la clé principale")),
        (Some(id), Some(main)) => report.fail(
            format!("{what} : {id}, alors que la clé principale de l'app est {main}"),
            Some("les mises à jour signées par la clé principale seraient refusées : embarque-la, ou passe par une version de transition"),
        ),
        (Some(id), None) => report.ok(format!("{what} : {id}")),
    }
}

fn check_tauri(
    project: &TauriProject,
    app: &AppSlug,
    server: Option<&str>,
    main_id: Option<KeyId>,
    report: &mut Report,
) {
    // Chaque réglage est lu comme le lit `tauri build`, OS par OS : un fichier de plateforme
    // (tauri.macos.conf.json…) peut le changer pour ses builds.
    let config = match TauriConfig::read(&project.config) {
        Ok(config) => config,
        Err(error) => {
            report.fail(format!("{error:#}"), None);
            return;
        }
    };
    let init_writes = format!("drakbase init {app} l'écrit");
    let init_enables = format!("drakbase init {app} l'active");
    for scoped in platform_settings(&config, tauri::ENDPOINTS) {
        check_feed(
            report,
            &endpoints(scoped.value),
            server.map(|server| project::tauri_endpoint(server, app)),
            app,
            &format!(
                "adresse de vérification (plugins.updater.endpoints){}",
                scoped.scope
            ),
            scoped.fix(&init_writes),
        );
    }
    for scoped in platform_settings(&config, tauri::PUBKEY) {
        let embedded = scoped
            .value
            .and_then(Value::as_str)
            .and_then(|text| project::key_id(text).ok());
        check_embedded(
            report,
            embedded,
            main_id,
            &format!("clé embarquée (plugins.updater.pubkey){}", scoped.scope),
            scoped.fix(&init_writes),
        );
    }
    for scoped in platform_settings(&config, "/plugins/updater/requireSignedVersion") {
        let scope = &scoped.scope;
        if scoped.value.and_then(Value::as_bool) == Some(true) {
            report.ok(format!(
                "version signée exigée (requireSignedVersion){scope}"
            ));
        } else {
            report.fail(
                format!(
                    "requireSignedVersion n'est pas activé{scope} : un serveur compromis pourrait présenter une ancienne version sous un numéro plus haut"
                ),
                Some(scoped.fix(&init_enables)),
            );
        }
    }
    for scoped in platform_settings(&config, "/bundle/createUpdaterArtifacts") {
        let scope = &scoped.scope;
        if scoped.value.and_then(Value::as_bool) == Some(true) {
            report.ok(format!(
                "archives de mise à jour (bundle.createUpdaterArtifacts){scope}"
            ));
        } else {
            report.fail(
                format!(
                    "bundle.createUpdaterArtifacts n'est pas activé{scope} : tauri build ne produira ni archives ni signatures"
                ),
                Some(scoped.fix(&init_enables)),
            );
        }
    }
    // L'installation Windows, telle que la lisent les builds Windows.
    let wrong = [
        ("/bundle/windows/nsis/installMode", "currentUser"),
        ("/plugins/updater/windows/installMode", "passive"),
    ]
    .into_iter()
    .map(|(pointer, wanted)| (config.get(Os::Windows, pointer), wanted))
    .find(|(setting, wanted)| setting.as_str() != Some(*wanted));
    if let Some((setting, _)) = wrong {
        let advice = if setting.source == project.config {
            format!("drakbase init {app} les règle")
        } else {
            PLATFORM_FIX.to_owned()
        };
        report.warn(
            format!(
                "{} : installation Windows à revoir (nsis.installMode currentUser, updater.windows.installMode passive)",
                project::display(setting.source)
            ),
            Some(&advice),
        );
    }

    let capabilities = project.src_tauri.join("capabilities/default.json");
    let granted: Vec<String> = Json::read(&capabilities)
        .ok()
        .and_then(|json| {
            json.get(&["permissions"])
                .and_then(Json::as_array)
                .map(<[Json]>::to_vec)
        })
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<&str> = project::TAURI_PERMISSIONS
        .iter()
        .copied()
        .filter(|permission| !granted.iter().any(|granted| granted == permission))
        .collect();
    if missing.is_empty() {
        report.ok("permissions des plugins (capabilities/default.json)");
    } else {
        report.warn(
            format!(
                "permissions absentes de {} : {}",
                project::display(&capabilities),
                missing.join(", ")
            ),
            Some("ajoute-les à la capacité de la fenêtre qui gère les mises à jour"),
        );
    }

    let cargo = fs::read_to_string(project.src_tauri.join("Cargo.toml")).unwrap_or_default();
    match project::cargo_version(&cargo, "tauri-plugin-updater") {
        Some(version) if version >= project::TAURI_UPDATER_MINIMUM => {
            report.ok(format!("tauri-plugin-updater {}.{}", version.0, version.1));
        }
        Some(version) => report.fail(
            format!(
                "tauri-plugin-updater {}.{} : requireSignedVersion demande {}.{} au moins",
                version.0,
                version.1,
                project::TAURI_UPDATER_MINIMUM.0,
                project::TAURI_UPDATER_MINIMUM.1
            ),
            Some("monte sa version dans Cargo.toml"),
        ),
        None if project::cargo_declares(&cargo, "tauri-plugin-updater") => {
            report.warn(
                "version de tauri-plugin-updater illisible dans Cargo.toml",
                None,
            );
        }
        None => report.fail(
            "tauri-plugin-updater absent de Cargo.toml",
            Some(&format!("drakbase init {app} l'ajoute")),
        ),
    }
    for (name, _) in &project::TAURI_CRATES[1..] {
        if !project::cargo_declares(&cargo, name) {
            report.warn(
                format!("{name} absent de Cargo.toml"),
                Some(&format!("drakbase init {app} l'ajoute")),
            );
        }
    }

    let package = Json::read(&project.root.join("package.json")).ok();
    let declared = |name: &str| {
        let (_, value) = project::npm_dependency(package.as_ref()?, name)?;
        Some(value)
    };
    if let Some(helper) = declared(project::HELPER_PACKAGE) {
        check_helper_range(helper, app, report);
    } else if declared(project::LEGACY_HELPER_PACKAGE).is_some() {
        report.warn(
            format!(
                "{} dans package.json : l'aide s'appelle désormais {}",
                project::LEGACY_HELPER_PACKAGE,
                project::HELPER_PACKAGE
            ),
            Some(&format!(
                "renomme la dépendance et les imports en {}, puis npm install",
                project::HELPER_PACKAGE
            )),
        );
    } else {
        report.warn(
            format!("{} absent de package.json", project::HELPER_PACKAGE),
            Some(&format!("drakbase init {app} l'ajoute, puis npm install")),
        );
    }

    let rust = project::find_files(&project.src_tauri.join("src"), |name| name.ends_with(".rs"));
    for (needle, what) in [
        ("tauri_plugin_updater", "le plugin updater"),
        ("tauri_plugin_process", "le plugin process (relaunch)"),
        ("tauri_plugin_os", "le plugin os (version du système)"),
    ] {
        if project::any_file_contains(&rust, needle) {
            report.ok(format!("{what} est enregistré dans le code Rust"));
        } else {
            report.warn(
                format!(
                    "{what} n'est pas enregistré dans {}/src",
                    project::display(&project.src_tauri)
                ),
                Some("ajoute-le au Builder de Tauri (clients/tauri/README.md, section 3)"),
            );
        }
    }
    let interface = project::find_files(&project.root.join("src"), |name| {
        [".ts", ".tsx", ".js", ".jsx", ".vue", ".svelte"]
            .iter()
            .any(|extension| name.ends_with(extension))
    });
    if project::any_file_contains(&interface, project::HELPER_PACKAGE) {
        report.ok(format!("l'interface utilise {}", project::HELPER_PACKAGE));
    } else if project::any_file_contains(&interface, project::LEGACY_HELPER_PACKAGE) {
        report.warn(
            format!(
                "l'interface importe encore {}",
                project::LEGACY_HELPER_PACKAGE
            ),
            Some(&format!(
                "remplace-le par {} dans les imports",
                project::HELPER_PACKAGE
            )),
        );
    } else {
        report.warn(
            format!(
                "l'interface n'utilise pas encore {}",
                project::HELPER_PACKAGE
            ),
            Some("useUpdater() ou checkForUpdate() (clients/tauri/README.md, section 4)"),
        );
    }
}

/// L'aide `@drakbase/tauri` déclarée dans `package.json` : prise par l'adresse du dépôt, sa plage
/// de versions doit admettre celle de l'outil, dont `init` ajoute les plugins. Déclarée autrement
/// (`file:`, `workspace:`, `link:`), elle ne se compare pas.
fn check_helper_range(declared: &Json, app: &AppSlug, report: &mut Report) {
    let name = project::HELPER_PACKAGE;
    let version = project::CLIENT_VERSION;
    let helper = declared.as_str().and_then(HelperRange::parse);
    // Un ancien dépôt, privé : l'app passe au dépôt public. `init` remplace l'adresse quand
    // l'aide est prise avec une plage ; sinon, c'est à la main.
    let legacy = declared.as_str().and_then(project::legacy_repository);
    if let Some(old) = legacy {
        let advice = if helper.is_some() {
            format!(
                "drakbase init {app} passe au dépôt public {}, puis npm install",
                project::REPOSITORY_PATH
            )
        } else {
            format!(
                "écris {} dans package.json, puis npm install",
                project::helper_dependency()
            )
        };
        report.warn(
            format!("{name} pris à l'ancien dépôt ({old})"),
            Some(&advice),
        );
    }
    match helper.map(|helper| (helper.fit(), helper.range)) {
        Some((RangeFit::Older, range)) => report.warn(
            format!(
                "{name} plus ancien que l'outil : la plage {range} de package.json s'arrête avant la version {version}"
            ),
            Some(&format!(
                "drakbase init {app} monte la plage, puis npm install"
            )),
        ),
        Some((RangeFit::Unreadable, range)) => report.warn(
            format!("{name} : plage « {range} » illisible dans package.json"),
            Some(&format!(
                "drakbase init n'y touche pas : vérifie qu'elle admet la version {version} de l'outil, ou écris {} à sa place, puis npm install",
                project::client_range()
            )),
        ),
        Some((RangeFit::Newer, range)) => report.warn(
            format!(
                "{name} plus récent que l'outil : la plage {range} de package.json commence après la version {version}"
            ),
            Some("mets l'outil drakbase à jour, puis relance drakbase doctor"),
        ),
        Some((RangeFit::Includes, _)) | None if legacy.is_none() => {
            report.ok(format!("{name} dans package.json"));
        }
        Some((RangeFit::Includes, _)) | None => {}
    }
}

fn check_swift(
    project: &SwiftProject,
    app: &AppSlug,
    server: Option<&str>,
    main_id: Option<KeyId>,
    report: &mut Report,
) {
    let xcode = matches!(project.manifest, SwiftManifest::Xcode(_));
    match &project.plist {
        None => report.fail(
            "aucun Info.plist trouvé dans le projet",
            Some("donne-le avec --plist ; pour un projet Xcode à l'Info.plist généré, ajoutes-en un (réglage « Info.plist File »)"),
        ),
        Some(path) => match read_plist(path) {
            None => report.fail(format!("{} illisible", project::display(path)), None),
            Some(info) => {
                report.ok(format!("Info.plist : {}", project::display(path)));
                check_info(&info, app, server, main_id, report);
                match info.get("CFBundleIdentifier").and_then(plist::Value::as_string) {
                    Some(id) if !id.contains("$(") => {
                        report.ok(format!("identifiant de bundle : {id}"));
                    }
                    Some(_) | None if xcode => report.note(
                        "identifiant de bundle réglé par Xcode : doctor --bundle <Nom.app> le contrôle sur l'app construite",
                    ),
                    _ => report.warn(
                        "identifiant de bundle absent ou illisible dans l'Info.plist",
                        Some("CFBundleIdentifier doit être celui de l'app sur le serveur"),
                    ),
                }
            }
        },
    }

    // Le produit tel que l'écrit chaque manifeste : `.product(name: "DrakBase", …)` dans
    // Package.swift, `productName = DrakBase;` dans le projet Xcode.
    let (manifest, product) = match &project.manifest {
        SwiftManifest::Package(path) => (path.clone(), format!("\"{}\"", project::SWIFT_PRODUCT)),
        SwiftManifest::Xcode(path) => (
            path.join("project.pbxproj"),
            format!("productName = {};", project::SWIFT_PRODUCT),
        ),
    };
    let manifest = std::slice::from_ref(&manifest);
    if project::any_file_contains(manifest, &product) {
        report.ok(format!("dépendance {} déclarée", project::SWIFT_PRODUCT));
    } else if project::any_file_contains(manifest, project::LEGACY_SWIFT_PRODUCT) {
        report.warn(
            format!(
                "dépendance {} : le paquet s'appelle désormais {}",
                project::LEGACY_SWIFT_PRODUCT,
                project::SWIFT_PRODUCT
            ),
            Some(&format!(
                ".product(name: \"{}\", package: \"{}\") à partir de la {}, et import {} dans le code",
                project::SWIFT_PRODUCT,
                project::SWIFT_PACKAGE,
                project::CLIENT_VERSION,
                project::SWIFT_PRODUCT
            )),
        );
    } else {
        report.warn(
            format!("dépendance {} absente du projet", project::SWIFT_PRODUCT),
            Some(&format!(
                ".package(url: \"{}\", from: \"{}\")",
                project::REPOSITORY_URL,
                project::CLIENT_VERSION
            )),
        );
    }
    // Un ancien dépôt, privé : l'app passe au dépôt public. SwiftPM tire le nom du paquet de son
    // adresse : l'adresse et `package:` changent ensemble.
    if let Some(old) = project::legacy_swift_package(manifest) {
        let advice = if xcode {
            format!(
                "dans Xcode, retire le paquet, puis ajoute-le par File › Add Package Dependencies…, adresse {}, produit {}",
                project::REPOSITORY_URL,
                project::SWIFT_PRODUCT
            )
        } else {
            format!(
                "dans Package.swift : .package(url: \"{}\", …) et .product(name: \"{}\", package: \"{}\")",
                project::REPOSITORY_URL,
                project::SWIFT_PRODUCT,
                project::SWIFT_PACKAGE
            )
        };
        report.warn(
            format!(
                "paquet {} pris à l'ancien dépôt ({old})",
                project::SWIFT_PRODUCT
            ),
            Some(&advice),
        );
    }
    let swift = project::find_files(&project.root, |name| name.ends_with(".swift"));
    for (needle, what, advice) in [
        (
            "RelaunchGate.waitForPreviousInstance",
            "le point d'entrée attend l'ancienne instance (RelaunchGate)",
            "appelle RelaunchGate.waitForPreviousInstance() dans un main, avant tout",
        ),
        (
            "Updater(",
            "l'app crée son Updater",
            "let updater = Updater(); updater.start() (clients/swift/DrakBase/README.md)",
        ),
    ] {
        if project::any_file_contains(&swift, needle) {
            report.ok(what);
        } else {
            report.warn(format!("introuvable dans le code : {what}"), Some(advice));
        }
    }
}

/// `DrakBaseFeedURL` et `DrakBasePublicKeys` d'un Info.plist, ou leurs anciens noms.
fn check_info(
    info: &plist::Dictionary,
    app: &AppSlug,
    server: Option<&str>,
    main_id: Option<KeyId>,
    report: &mut Report,
) {
    for key in project::INFO_KEYS {
        if key.only_legacy(info) {
            report.warn(
                format!("{} : ancien nom de {}", key.legacy, key.name),
                Some(&format!("drakbase init {app} la renomme")),
            );
        }
    }
    let feed: Vec<String> = project::FEED_KEY
        .get(info)
        .and_then(plist::Value::as_string)
        .map(str::to_owned)
        .into_iter()
        .collect();
    let init_writes = format!("drakbase init {app} l'écrit");
    check_feed(
        report,
        &feed,
        server.map(|server| project::swift_feed(server, app)),
        app,
        "adresse de vérification (DrakBaseFeedURL)",
        &init_writes,
    );
    let keys: Vec<KeyId> = project::PUBLIC_KEYS_KEY
        .get(info)
        .and_then(plist::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(plist::Value::as_string)
                .filter_map(|key| project::key_id(key).ok())
                .collect()
        })
        .unwrap_or_default();
    check_embedded(
        report,
        keys.first().copied(),
        main_id,
        "clé embarquée en premier (DrakBasePublicKeys)",
        &init_writes,
    );
    if keys.len() < 2 {
        report.warn(
            "DrakBasePublicKeys n'a pas de clé de secours",
            Some("ajoute la clé publique de secours après la principale"),
        );
    }
}

/// Une stack : `stack.json`, Docker, et le compose, lu par Docker Compose et contrôlé comme à la
/// publication, avec des empreintes provisoires. `host` : l'hôte du serveur connecté.
fn check_stack(
    project: &StackProject,
    app: &AppSlug,
    host: Option<&str>,
    main_id: Option<KeyId>,
    report: &mut Report,
) {
    let name = project::display(&project.stack_json());
    let manifest = match project.manifest() {
        Ok(manifest) => {
            report.ok(format!(
                "{name} : service {}, plateformes {}, retour arrière {}{}",
                manifest.service,
                stack::platform_list(&manifest.platforms),
                manifest.rollback.as_str(),
                manifest
                    .pre_update
                    .as_ref()
                    .map(|service| format!(", {service} avant chaque mise à jour"))
                    .unwrap_or_default()
            ));
            Some(manifest)
        }
        Err(error) => {
            report.fail(
                format!("{error:#}"),
                Some(&format!(
                    "drakbase init {app} crée stack.json et y écrit les clés ; ses champs sont dans docs/PROTOCOLE.md, « stack.json »"
                )),
            );
            None
        }
    };
    if let Some(manifest) = &manifest {
        let keys: Vec<KeyId> = manifest.trusted_key_ids();
        check_embedded(
            report,
            keys.first().copied(),
            main_id,
            "clé embarquée en premier (public_keys de stack.json)",
            &format!("drakbase init {app} l'écrit"),
        );
        if keys.len() < 2 {
            report.warn(
                "public_keys de stack.json n'a pas de clé de secours",
                Some("ajoute la clé publique de secours après la principale"),
            );
        }
    }
    match project.env_file() {
        Some(path) => report.ok(format!(
            "{} : les réglages de la machine",
            project::display(&path)
        )),
        None => report.note(format!(
            "pas de {} : la machine n'aura aucun réglage à changer",
            project::display(&project.env_example())
        )),
    }

    report.section("Docker");
    let tools = Tools::detect();
    match tools.require() {
        Ok(versions) => report.ok(versions),
        Err(error) => report.fail(
            format!("{error:#}"),
            Some("drakbase release construit les images avec docker buildx, et lit le compose avec docker compose"),
        ),
    }
    let (Some(manifest), Ok(())) = (manifest, tools.require_compose()) else {
        return;
    };
    let production = ServerUrl::production();
    let host = host.unwrap_or(production.image_host());
    match stack::examine(project, &manifest, app, host, None) {
        Ok(examination) => {
            let images: Vec<String> = examination
                .plan
                .images
                .iter()
                .map(|image| format!("{} ({})", image.name, image.services().join(", ")))
                .collect();
            report.ok(format!(
                "compose lu par Docker Compose : {} image(s) à construire, {}",
                images.len(),
                images.join(", ")
            ));
            for note in &examination.plan.notes {
                report.note(note);
            }
            report.ok("compose publié conforme aux règles des stacks");
            for permission in &examination.report.permissions {
                report.note(format!("demandé à la machine : {permission}"));
            }
            for warning in &examination.report.warnings {
                report.warn(
                    warning.to_string(),
                    Some("le projet Compose porte le nom de l'app, et ses conteneurs en tirent le leur"),
                );
            }
        }
        Err(error) => report.fail(format!("{error:#}"), None),
    }
}

/// Une app cli : `drakbase.json`, son app et ses clés.
fn check_cli(project: &CliProject, app: &AppSlug, main_id: Option<KeyId>, report: &mut Report) {
    let name = project::display(&project.file);
    let manifest = match CliManifest::read(&project.file) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.fail(
                format!("{error:#}"),
                Some(&format!("drakbase init {app} --cli l'écrit")),
            );
            return;
        }
    };
    match &manifest.app {
        Some(found) if *found == app.to_string() => report.ok(format!("{name} : app {found}")),
        Some(found) => report.fail(
            format!("{name} est celui de l'app {found}, pas de {app}"),
            Some("vérifie l'identifiant de l'app"),
        ),
        None => report.warn(
            format!("{name} ne dit pas l'app"),
            Some(&format!("drakbase init {app} --cli l'écrit")),
        ),
    }
    let keys = manifest.key_ids();
    check_embedded(
        report,
        keys.first().copied(),
        main_id,
        "clé embarquée en premier (public_keys de drakbase.json)",
        &format!("drakbase init {app} l'écrit"),
    );
    if keys.len() < 2 {
        report.warn(
            "public_keys de drakbase.json n'a pas de clé de secours",
            Some("ajoute la clé publique de secours après la principale"),
        );
    }
}

fn check_bundle(
    bundle: &Path,
    app: &AppSlug,
    server: Option<&str>,
    main_id: Option<KeyId>,
    facts: &mut Facts,
    report: &mut Report,
) {
    match pack::require_certificate(bundle) {
        Ok(authority) => report.ok(format!(
            "{} est signé par « {authority} »",
            bundle.display()
        )),
        Err(error) => report.fail(format!("{error:#}"), None),
    }
    match read_plist(&bundle.join("Contents/Info.plist")) {
        None => report.fail(
            format!("{}/Contents/Info.plist illisible", bundle.display()),
            None,
        ),
        Some(info) => {
            check_info(&info, app, server, main_id, report);
            let text = |key: &str| {
                info.get(key)
                    .and_then(plist::Value::as_string)
                    .map(str::to_owned)
            };
            if let Some(version) =
                text("CFBundleShortVersionString").and_then(|text| project::literal_version(&text))
            {
                facts.version = Some(version);
            }
            if facts.kind.is_none() {
                facts.kind = Some(AppKind::Swift);
            }
            if facts.bundle_id.is_none() {
                facts.bundle_id = text("CFBundleIdentifier");
            }
        }
    }
}

fn check_server(
    connection: &Connection,
    app: &AppSlug,
    facts: &Facts,
    main_id: Option<KeyId>,
    report: &mut Report,
) {
    if let Some(problem) = connection.other_app(app) {
        report.fail(problem, Some(OTHER_APP_ADVICE));
    }
    let info = match connection.api.app(app) {
        Ok(info) => info,
        Err(error) if error.code() == Some("not_found") => {
            let advice = match facts.kind {
                Some(kind @ (AppKind::Stack | AppKind::Cli)) => format!(
                    "drakbase init {app}, à la racine du projet, ouvre l'administration pré-remplie pour la créer (type {}) avec sa clé",
                    kind.as_str()
                ),
                kind => format!(
                    "drakbase init {app}, à la racine du projet, ouvre l'administration pré-remplie pour la créer (type {}, identifiant de bundle {}) avec sa clé",
                    match kind {
                        Some(AppKind::Tauri) => "Tauri",
                        Some(AppKind::Swift) => "Swift",
                        _ => "Swift ou Tauri",
                    },
                    facts.bundle_id.as_deref().unwrap_or("celui de l'app")
                ),
            };
            report.fail(
                format!("l'app {app} n'existe pas sur le serveur"),
                Some(&advice),
            );
            return;
        }
        Err(error) => {
            report.fail(format!("lecture de l'app {app} : {error}"), None);
            return;
        }
    };
    report.ok(format!("app « {} » sur le serveur", info.name));
    if let Some(local) = facts.kind
        && local != info.kind
    {
        report.fail(
            format!(
                "l'app est de type {} sur le serveur, et le projet est {}",
                info.kind.as_str(),
                api::described(local)
            ),
            Some("vérifie l'identifiant de l'app"),
        );
    }
    match (&facts.bundle_id, &info.bundle_id) {
        (Some(local), Some(remote)) if local != remote => report.fail(
            format!("identifiant de bundle {local}, alors que le serveur attend {remote}"),
            Some("le serveur refuserait les archives : corrige l'un ou l'autre"),
        ),
        (Some(_), None) => report.warn(
            "le serveur n'a pas d'identifiant de bundle pour cette app : il ne contrôlera pas celui des archives",
            Some("renseigne-le dans l'administration"),
        ),
        _ => {}
    }
    if let Some(main) = main_id {
        let hex = main.to_hex();
        match info.keys.iter().find(|key| key.key_id.eq_ignore_ascii_case(&hex)) {
            Some(key) if key.status == KeyStatus::Active => report.ok(format!("clé {main} active sur le serveur")),
            Some(key) if key.status == KeyStatus::Next => report.warn(
                format!("clé {main} déclarée comme suivante, pas encore active"),
                Some("active-la dans l'administration quand la version de transition est publiée"),
            ),
            Some(_) => report.fail(
                format!("clé {main} retirée sur le serveur : il refusera les versions qu'elle signe"),
                None,
            ),
            None => report.fail(
                format!("la clé principale {main} n'est pas déclarée sur le serveur"),
                Some(&format!("drakbase init {app}, à la racine du projet, ouvre l'administration pré-remplie pour la déclarer")),
            ),
        }
    }
    let stable = info.latest.of(Channel::Stable);
    let beta = info.latest.of(Channel::Beta);
    report.ok(format!(
        "dernières versions publiées : stable {}, bêta {}",
        stable.map_or("aucune".to_owned(), ToString::to_string),
        beta.map_or("aucune".to_owned(), ToString::to_string)
    ));
    if let Some(version) = &facts.version {
        check_version(version, stable, beta, report);
    }
}

/// La version du projet face aux dernières versions publiées, comme `publish` la jugera : une
/// pré-publication (`1.3.0-beta.1`) va sur le canal bêta, une autre version sur le canal stable,
/// et chacune doit dépasser la dernière de son canal.
fn check_version(
    version: &Version,
    stable: Option<&Version>,
    beta: Option<&Version>,
    report: &mut Report,
) {
    let prerelease = version.is_prerelease();
    let (channel, latest, other_channel, other) = if prerelease {
        ("bêta", beta, "stable", stable)
    } else {
        ("stable", stable, "bêta", beta)
    };
    match latest {
        Some(latest) if version <= latest => report.warn(
            format!(
                "la version du projet, {version}, n'est pas plus haute que la dernière version {channel} publiée, {latest}"
            ),
            Some("monte-la avant de publier : sinon l'outil refusera, et les apps déjà à jour ne la prendraient pas"),
        ),
        // Publiée sur l'autre canal, la version existe déjà : le serveur la refuserait.
        _ if other == Some(version) => report.warn(
            format!("la version du projet, {version}, est déjà publiée en {other_channel}"),
            Some(if prerelease {
                "choisis une version plus haute : l'outil refusera celle-ci"
            } else {
                "pour la passer en stable, change son canal dans l'administration, sans rien renvoyer ; sinon, monte la version"
            }),
        ),
        // Une bêta plus basse que la dernière stable se publie, mais personne ne la reçoit.
        _ if prerelease && stable.is_some_and(|stable| version <= stable) => report.warn(
            format!(
                "la version du projet, {version}, est une pré-publication plus basse que la dernière version stable, {}",
                stable.map(ToString::to_string).unwrap_or_default()
            ),
            Some("les apps réglées sur bêta reçoivent aussi les versions stables : elles garderaient la stable, sans recevoir celle-ci"),
        ),
        _ => report.ok(format!("version du projet : {version}")),
    }
}

/// Noms des certificats de signature de code du trousseau, valides ou non : un certificat
/// auto-signé est « non approuvé », ce qui ne gêne ni codesign ni les mises à jour.
fn codesigning_identities() -> Result<Vec<String>> {
    if let Some(list) = env::var_os(IDENTITIES_VAR) {
        return Ok(list
            .to_string_lossy()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect());
    }
    let output = Command::new("/usr/bin/security")
        .args(["find-identity", "-p", "codesigning"])
        .output()
        .context("security find-identity")?;
    let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split('"').nth(1))
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn check_codesign(facts: &Facts, report: &mut Report) {
    let names = match codesigning_identities() {
        Ok(names) => names,
        Err(error) => {
            report.warn(format!("liste des certificats illisible : {error:#}"), None);
            return;
        }
    };
    let swift = facts.kind == Some(AppKind::Swift);
    if names.is_empty() {
        let text = "aucun certificat de signature de code dans le trousseau";
        let advice = "crée-le dans Trousseau d'accès (docs/PLAN.md, « Clés et certificat ») : une app Swift signée ad hoc ne reçoit aucune mise à jour";
        if swift {
            report.fail(text, Some(advice));
        } else {
            report.warn(text, Some(advice));
        }
        return;
    }
    // Le certificat des builds macOS d'une app Tauri, et le fichier qui le donne : dans un fichier
    // de plateforme, il se corrige là, `init --sign-id` n'y écrit pas.
    let source = facts.signing_source.as_deref().unwrap_or("tauri.conf.json");
    let set_it = if facts.signing_in_platform_file {
        format!("donne le nom de ton certificat dans {source}")
    } else {
        "drakbase init <app> --sign-id \"<nom du certificat>\"".to_owned()
    };
    match facts.signing_identity.as_deref() {
        Some("-") => report.warn(
            format!("bundle.macOS.signingIdentity vaut « - » dans {source} : l'app macOS sera signée ad hoc"),
            Some(&set_it),
        ),
        Some(name) if !names.iter().any(|known| known == name) => report.fail(
            format!("le certificat « {name} » de {source} n'est pas dans le trousseau"),
            Some(&format!("certificats présents : {}", names.join(", "))),
        ),
        Some(name) => report.ok(format!("certificat « {name} » présent")),
        None if facts.kind == Some(AppKind::Tauri) => report.warn(
            "bundle.macOS.signingIdentity absent : l'app macOS ne sera pas signée par ton certificat",
            Some(&set_it),
        ),
        None => report.ok(format!("certificat(s) : {}", names.join(", "))),
    }
}

fn check_scripts(root: &Path, report: &mut Report) {
    let scripts = project::find_files(root, |name| name.ends_with(".sh"));
    let mut clean = true;
    for script in &scripts {
        let Ok(text) = fs::read_to_string(script) else {
            continue;
        };
        for (line, name) in project::glued_variables(&text) {
            clean = false;
            report.warn(
                format!("{}:{line} : ${name} est collé à un caractère non ASCII", project::display(script)),
                Some(&format!(
                    "dans une locale UTF-8, le bash de macOS lit une autre variable : écris ${{{name}}}"
                )),
            );
        }
    }
    if clean {
        report.ok(format!("{} script(s) sans piège de locale", scripts.len()));
    }
}
