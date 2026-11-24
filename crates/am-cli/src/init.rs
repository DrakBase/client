//! `drakbase init <app>` : branche les mises à jour de DrakBase - Apps Market dans le projet du dossier
//! courant : une app Tauri, une app Swift, une stack Docker (`--stack`) ou une app cli (`--cli`).
//!
//! - Crée les clés de l'app si aucune n'existe, comme `keygen` : sauvegardées dans KeePassXC, le
//!   mot de passe de la clé principale dans le trousseau.
//! - App Tauri : `tauri.conf.json` (plugin updater, archives de mise à jour, installation
//!   Windows), les permissions de `capabilities/default.json`, les plugins de `Cargo.toml` et les
//!   paquets de `package.json`. L'aide `@drakbase/tauri` déjà déclarée par l'adresse du dépôt suit
//!   la version de l'outil : une plage qui s'arrête avant elle est montée. Un fichier de
//!   plateforme (`tauri.macos.conf.json`…) n'est jamais modifié : s'il change un de ces réglages
//!   pour ses builds, `init` le dit.
//! - App Swift : `DrakBaseFeedURL` et `DrakBasePublicKeys` dans l'Info.plist ; les clés écrites
//!   sous leur ancien nom (`AppsMarket…`) sont renommées, valeurs inchangées.
//! - Stack Docker : `public_keys` dans `stack/stack.json`, créé s'il manque (squelette à
//!   vérifier : service surveillé, plateformes).
//! - App cli : `drakbase.json` à la racine, avec l'app et ses clés publiques, que l'exécutable lit
//!   à la compilation.
//! - Déclare l'app et sa clé sur le serveur, si l'outil y est connecté : l'administration s'ouvre
//!   sur une page pré-remplie, à valider (`register`).
//! - Affiche ensuite ce qui reste à faire à la main, le code et la publication, puis les contrôles
//!   de `doctor` qui ne passent pas encore. Le code source n'est jamais modifié : seuls les
//!   fichiers de configuration le sont.
//!
//! Relancer la commande ne change que ce qui manque. Une clé déjà embarquée et différente n'est
//! jamais remplacée : changer de clé demande une version de transition.

use std::fs;
use std::iter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::Os;
use am_protocol::signature::KeyId;
use am_protocol::stack::is_service_name;
use am_protocol::stack::manifest::{DEFAULT_PLATFORMS, StackManifest};
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::AppKind;
use crate::backup;
use crate::cli_project::{self, CliProject};
use crate::connection::{ConnectError, Connection};
use crate::credentials::{Secrets, ServerUrl, parse_server};
use crate::doctor;
use crate::files;
use crate::home::{AppSlug, Home, HomeArgs, KeyRole};
use crate::keepassxc::VaultArgs;
use crate::keygen;
use crate::project::{
    self, HelperRange, Hints, Json, Project, RangeFit, SwiftManifest, SwiftProject, TauriProject,
};
use crate::register::Registration;
use crate::stack::{self, StackProject};
use crate::tauri::{self, TauriConfig};

#[derive(clap::Args)]
pub struct Args {
    /// Identifiant de l'app sur le serveur (slug), par exemple translate ; il ne change plus
    #[arg(value_name = "APP")]
    app: AppSlug,

    /// Adresse du serveur [défaut : celle de la connexion enregistrée par « drakbase login »]
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// Info.plist de l'app Swift à compléter [défaut : le seul Info.plist du projet]
    #[arg(long, value_name = "FICHIER")]
    plist: Option<PathBuf>,

    /// Configuration de l'app Tauri, tauri.conf.json ou son dossier [défaut :
    /// src-tauri/tauri.conf.json ou tauri.conf.json du dossier courant]
    #[arg(long = "tauri-config", value_name = "CHEMIN")]
    tauri_config: Option<PathBuf>,

    /// Certificat de signature de code des builds macOS d'une app Tauri
    /// (bundle.macOS.signingIdentity)
    #[arg(long, value_name = "NOM")]
    sign_id: Option<String>,

    /// Une stack Docker : son dossier, avec compose.yaml [défaut : stack]
    #[arg(
        long,
        value_name = "DOSSIER",
        num_args = 0..=1,
        default_missing_value = stack::DEFAULT_DIR,
        conflicts_with_all = ["plist", "tauri_config", "sign_id", "cli"]
    )]
    stack: Option<PathBuf>,

    /// Une app cli (exécutable Linux) : drakbase.json à la racine du projet, avec ses clés
    /// publiques
    #[arg(long, conflicts_with_all = ["plist", "tauri_config", "sign_id"])]
    cli: bool,

    /// Lit sur l'entrée standard le mot de passe de la base KeePassXC, s'il faut créer les clés,
    /// plutôt qu'au terminal
    #[arg(long)]
    password_stdin: bool,

    #[command(flatten)]
    vault: VaultArgs,

    #[command(flatten)]
    home: HomeArgs,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let app = &args.app;
    let secrets = Secrets::select(&home)?;
    let server = server_address(&secrets, args.server.as_deref())?;
    let hints = Hints {
        tauri_config: args.tauri_config.as_deref(),
        plist: args.plist.as_deref(),
        stack: args.stack.as_deref(),
        cli: args.cli,
    };
    let project = project::detect(Path::new(""), &hints)?.context(
        "le dossier courant n'est pas un projet d'app reconnu : ni src-tauri/tauri.conf.json (ou tauri.conf.json), ni Package.swift, ni projet Xcode, ni stack/compose.yaml, ni drakbase.json. Lance la commande à la racine du projet, ou donne --tauri-config, --stack <dossier> (stack Docker) ou --cli (exécutable Linux).",
    )?;
    match &project {
        Project::Tauri(tauri) => {
            println!("App Tauri : {}", project::display(&tauri.config));
        }
        Project::Swift(swift) => {
            let manifest = match &swift.manifest {
                SwiftManifest::Package(path) | SwiftManifest::Xcode(path) => project::display(path),
            };
            let plist = swift
                .plist
                .as_deref()
                .map_or("aucun".to_owned(), project::display);
            println!("App Swift : {manifest}, Info.plist : {plist}");
        }
        Project::Stack(stack) => {
            println!("Stack Docker : {}", project::display(&stack.dir));
        }
        Project::Cli(cli) => {
            println!("App cli   : {}", project::display(&cli.file));
        }
    }
    println!("Serveur   : {server}");
    println!("App       : {app}");

    let keys = ensure_keys(&home, app, &args.vault, args.password_stdin)?;
    println!();
    println!("Configuration :");
    let identity = match &project {
        Project::Tauri(tauri) => {
            configure_tauri(tauri, app, server.as_str(), &keys, args.sign_id.as_deref())?
        }
        Project::Swift(swift) => configure_swift(swift, app, server.as_str(), &keys)?,
        Project::Stack(stack) => configure_stack(stack, app, &keys)?,
        Project::Cli(cli) => configure_cli(cli, app, &keys)?,
    };

    println!();
    println!("Serveur :");
    let registration = Registration {
        app,
        name: identity.name,
        kind: match &project {
            Project::Tauri(_) => AppKind::Tauri,
            Project::Swift(_) => AppKind::Swift,
            Project::Stack(_) => AppKind::Stack,
            Project::Cli(_) => AppKind::Cli,
        },
        bundle_id: identity.bundle_id,
        public_key: &keys.main,
        key_id: keys.main_id,
    };
    let declared = match declare(&secrets, &server, &registration) {
        Ok(declared) => Some(declared),
        Err(error) => {
            println!("✗ {}", crate::describe(&error));
            None
        }
    };

    println!();
    println!("Reste à faire :");
    let mut step = Steps(0);
    match &project {
        Project::Tauri(tauri) => tauri_steps(&mut step, tauri, app),
        Project::Swift(swift) => swift_steps(&mut step, swift),
        Project::Stack(stack) => stack_steps(&mut step, stack),
        Project::Cli(_) => cli_steps(&mut step),
    }
    match declared {
        Some(true) => {}
        Some(false) => step.next(&format!(
            "Déclarer l'app sur le serveur : connecte l'outil (« drakbase login --server {server} »), puis relance « drakbase init {app} », qui ouvre l'administration pré-remplie."
        )),
        None => step.next(&format!(
            "Régler le problème du serveur ci-dessus, puis relancer « drakbase init {app} »."
        )),
    }
    match &project {
        Project::Tauri(_) => step.next(&format!(
            "Publier, depuis ce dossier : drakbase publish --app {app} --version <version> --channel stable --notes notes.md --publish <artefacts de tauri build>"
        )),
        Project::Swift(_) => step.next(&format!(
            "Publier : drakbase release <Nom.app> --app {app} --channel stable --notes notes.md --publish"
        )),
        Project::Stack(stack) => step.next(&format!(
            "Publier, depuis ce dossier : drakbase release --app {app} --stack {} --version <version> --channel stable --notes notes.md --publish",
            project::display(&stack.dir)
        )),
        Project::Cli(_) => {
            step.next(&format!(
                "Publier, une fois les exécutables construits pour chaque arch (aarch64, x86_64…) et signés (drakbase sign <exécutable> --app {app} --version <version>) :"
            ));
            step.code(&[&format!(
                "drakbase publish --app {app} --version <version> --channel stable --notes notes.md --publish linux/aarch64/bin=<exécutable arm64> linux/x86_64/bin=<exécutable x86_64>"
            )]);
        }
    }

    println!();
    println!("Contrôle final, comme « drakbase doctor » :");
    doctor::check_quietly(
        &home,
        &doctor::Options {
            app: Some(app.clone()),
            plist: args.plist.clone(),
            tauri_config: args.tauri_config.clone(),
            bundle: None,
            stack: args.stack.clone(),
            cli: args.cli,
        },
    )?;
    Ok(if declared.is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Déclare l'app sur le serveur, si l'outil y est connecté ; rend `false` s'il ne l'est pas.
fn declare(secrets: &Secrets, server: &ServerUrl, registration: &Registration) -> Result<bool> {
    let credentials = match secrets.login()? {
        Some(credentials) if credentials.server == *server => credentials,
        _ => {
            println!(
                "· l'outil n'est pas connecté à {server} : l'app n'y est pas déclarée pour l'instant"
            );
            return Ok(false);
        }
    };
    let connection = Connection::connect(credentials).map_err(ConnectError::into_error)?;
    registration.ensure(&connection)?;
    Ok(true)
}

/// Étapes numérotées de la fin du compte rendu.
struct Steps(usize);

impl Steps {
    fn next(&mut self, text: &str) {
        self.0 += 1;
        println!("{:>2}. {text}", self.0);
    }

    /// Lignes de code ou de commande, sous l'étape.
    fn code(&self, lines: &[&str]) {
        for line in lines {
            println!("      {line}");
        }
    }
}

/// L'adresse du serveur : `--server`, sinon celle de la connexion enregistrée par `login`.
fn server_address(secrets: &Secrets, given: Option<&str>) -> Result<ServerUrl> {
    if let Some(text) = given {
        return parse_server(text).context("--server");
    }
    match secrets.login()? {
        Some(credentials) => Ok(credentials.server),
        None => bail!(
            "aucune connexion enregistrée dans {} : connecte d'abord l'outil (« drakbase login --server https://… »), ou donne l'adresse du serveur avec --server",
            secrets.login_place()
        ),
    }
}

/// Les clés publiques de l'app, à embarquer.
struct Keys {
    main: String,
    main_id: KeyId,
    backup: Option<String>,
}

/// Ce que la configuration du projet dit de l'app, pour la déclarer sur le serveur.
struct Identity {
    bundle_id: Option<String>,
    name: Option<String>,
}

/// Lit les clés de l'app, après les avoir créées si aucune n'existe. Un jeu incomplet n'est pas
/// complété : il vaut mieux comprendre d'abord ce qui manque.
fn ensure_keys(
    home: &Home,
    app: &AppSlug,
    vault: &VaultArgs,
    password_stdin: bool,
) -> Result<Keys> {
    let main = home.key_files(app, KeyRole::Main);
    let backup = home.key_files(app, KeyRole::Backup);
    let none_exist = [&main.secret, &main.public, &backup.secret, &backup.public]
        .iter()
        .all(|path| path.symlink_metadata().is_err());
    if none_exist {
        println!();
        println!("Aucune clé pour l'app {app} : création, comme « drakbase keygen {app} ».");
        keygen::create(home, app, vault, password_stdin)?;
    }
    let public = fs::read_to_string(&main.public).with_context(|| {
        format!(
            "clé publique principale {} : jeu de clés incomplet, à restaurer depuis ta sauvegarde",
            main.public.display()
        )
    })?;
    if !main.secret.is_file() {
        bail!(
            "clé secrète principale {} introuvable : sans elle, aucune version ne pourra être signée. Restaure-la depuis ta sauvegarde.",
            main.secret.display()
        );
    }
    let main_id = project::key_id(&public).with_context(|| main.public.display().to_string())?;
    let backup_public = fs::read_to_string(&backup.public)
        .ok()
        .map(|text| text.trim().to_owned());
    println!();
    match &backup_public {
        Some(text) => println!(
            "Clés     : principale {main_id}, secours {}",
            project::key_id(text).map_or("illisible".to_owned(), |id| id.to_hex())
        ),
        None => println!("Clés     : principale {main_id}, pas de clé de secours"),
    }
    match backup::load(home, app)? {
        Some(record) if record.main == main_id.to_hex() => println!(
            "Sauvegarde : KeePassXC ({}), vérifiée le {}",
            record.keepassxc.display(),
            record.day()
        ),
        _ => println!(
            "! clés jamais sauvegardées hors de ce Mac : « drakbase backup {app} » les range dans KeePassXC"
        ),
    }
    Ok(Keys {
        main: public.trim().to_owned(),
        main_id,
        backup: backup_public,
    })
}

/// « ✓ fichier : ce qui a changé », ou « · fichier : déjà en place ».
fn report(file: &str, changes: &[String]) {
    if changes.is_empty() {
        println!("· {file} : déjà en place");
    } else {
        println!("✓ {file} : {}", changes.join(", "));
    }
}

/// Configure l'app Tauri ; rend son identifiant de bundle et son nom.
fn configure_tauri(
    project: &TauriProject,
    app: &AppSlug,
    server: &str,
    keys: &Keys,
    sign_id: Option<&str>,
) -> Result<Identity> {
    let name = project::display(&project.config);
    let mut config = Json::read(&project.config)?;
    // Les fichiers de plateforme sont lus avant d'écrire quoi que ce soit : illisibles, ils
    // arrêtent tout.
    let platforms = TauriConfig::read(&project.config)?;
    if let Some(existing) = config
        .get(&["plugins", "updater", "pubkey"])
        .and_then(Json::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        let existing_id = project::key_id(existing).ok();
        if existing_id != Some(keys.main_id) {
            bail!(
                "{name} embarque déjà une autre clé publique ({}) : je ne la remplace pas. Changer de clé demande une version de transition (clients/tauri/README.md, « Changer de clé »).",
                existing_id.map_or("illisible".to_owned(), |id| id.to_hex())
            );
        }
    }
    let endpoint = project::tauri_endpoint(server, app);
    let mut changes: Vec<String> = Vec::new();
    if let Some(others) = config
        .get(&["plugins", "updater", "endpoints"])
        .and_then(Json::as_array)
        .filter(|items| {
            items
                .iter()
                .any(|item| item.as_str() != Some(endpoint.as_str()))
        })
    {
        let replaced: Vec<&str> = others.iter().filter_map(Json::as_str).collect();
        println!(
            "! {name} : adresses de vérification remplacées ({})",
            replaced.join(", ")
        );
    }
    // Chaque réglage, son nom dans le compte rendu, et l'OS dont il règle les builds s'il n'y en
    // a qu'un.
    let settings: [(&[&str], Json, &str, Option<Os>); 6] = [
        (
            &["plugins", "updater", "pubkey"],
            Json::string(&keys.main),
            "clé publique",
            None,
        ),
        (
            &["plugins", "updater", "endpoints"],
            Json::Array(vec![Json::string(&endpoint)]),
            "adresse de vérification",
            None,
        ),
        (
            &["plugins", "updater", "requireSignedVersion"],
            Json::Bool(true),
            "version signée exigée",
            None,
        ),
        (
            &["plugins", "updater", "windows", "installMode"],
            Json::string("passive"),
            "mise à jour Windows sans fenêtre",
            Some(Os::Windows),
        ),
        (
            &["bundle", "createUpdaterArtifacts"],
            Json::Bool(true),
            "archives de mise à jour",
            None,
        ),
        (
            &["bundle", "windows", "nsis", "installMode"],
            Json::string("currentUser"),
            "installation Windows sans droits d'administrateur",
            Some(Os::Windows),
        ),
    ];
    for (keys_path, value, label, _) in &settings {
        if config.set(keys_path, value.clone())? {
            changes.push((*label).to_owned());
        }
    }
    // Le certificat des builds macOS. Si tauri.macos.conf.json le donne déjà, c'est lui que lit
    // `tauri build` : écrit dans tauri.conf.json, le certificat demandé resterait sans effet.
    let mut masked_identity = None;
    if let Some(identity) = sign_id {
        let effective = platforms.get(Os::Darwin, tauri::SIGNING_IDENTITY);
        if effective.source == project.config {
            if config.set(
                &["bundle", "macOS", "signingIdentity"],
                Json::string(identity),
            )? {
                changes.push(format!("certificat macOS « {identity} »"));
            }
        } else if effective.as_str() != Some(identity) {
            masked_identity = Some((
                project::display(effective.source),
                effective.as_str().map(str::to_owned),
            ));
        }
    }
    if !changes.is_empty() {
        config.write(&project.config)?;
    }
    report(&name, &changes);

    // Ce que lisent les builds de chaque OS, une fois les fichiers de plateforme fusionnés.
    let merged = TauriConfig::read(&project.config)?;
    if let (Some(identity), Some((file, found))) = (sign_id, &masked_identity) {
        println!(
            "! {file} donne déjà bundle.macOS.signingIdentity pour les builds macOS ({}) : « {identity} » n'est pas écrit dans {name}, où il resterait sans effet. Change-le dans {file}.",
            found
                .as_ref()
                .map_or("aucun certificat".to_owned(), |found| format!(
                    "« {found} »"
                ))
        );
    }
    for os in tauri::OSES {
        for (keys_path, wanted, label, only) in &settings {
            if only.is_some_and(|only| only != os) {
                continue;
            }
            let setting = merged.get(os, &format!("/{}", keys_path.join("/")));
            if setting.source == project.config
                || setting.value == Some(&serde_json::to_value(wanted)?)
            {
                continue;
            }
            let found = match setting.value {
                None => "rien".to_owned(),
                Some(value) if *label == "clé publique" => value
                    .as_str()
                    .and_then(|key| project::key_id(key).ok())
                    .map_or("une clé illisible".to_owned(), |id| format!("la clé {id}")),
                Some(value) => value.to_string(),
            };
            println!(
                "! {} change « {label} » pour les builds {} ({} y vaut {found}) : corrige ce fichier, ou retires-en ce réglage. drakbase init ne modifie que {name}.",
                project::display(setting.source),
                tauri::os_name(os),
                keys_path.join(".")
            );
        }
    }
    // L'identifiant et le nom sont ceux des builds macOS : le serveur ne contrôle que
    // l'identifiant des archives macOS.
    let text = |pointer: &str| merged.get(Os::Darwin, pointer).as_str().map(str::to_owned);
    let identity = Identity {
        bundle_id: text("/identifier"),
        name: text("/productName"),
    };

    let capabilities = project.src_tauri.join("capabilities/default.json");
    if capabilities.is_file() {
        let mut json = Json::read(&capabilities)?;
        let mut permissions = match json.get(&["permissions"]) {
            Some(Json::Array(items)) => items.clone(),
            Some(_) => bail!(
                "{} : permissions doit être un tableau",
                project::display(&capabilities)
            ),
            None => Vec::new(),
        };
        let mut added = Vec::new();
        for permission in project::TAURI_PERMISSIONS {
            if !permissions
                .iter()
                .any(|item| item.as_str() == Some(permission))
            {
                permissions.push(Json::string(permission));
                added.push(permission.to_owned());
            }
        }
        if !added.is_empty() {
            json.set(&["permissions"], Json::Array(permissions))?;
            json.write(&capabilities)?;
        }
        report(&project::display(&capabilities), &added);
    } else {
        println!(
            "! {} introuvable : donne à la fenêtre qui gère les mises à jour les permissions {}",
            project::display(&capabilities),
            project::TAURI_PERMISSIONS.join(", ")
        );
    }

    let cargo = project.src_tauri.join("Cargo.toml");
    if cargo.is_file() {
        let text = fs::read_to_string(&cargo)
            .with_context(|| format!("lecture de {}", project::display(&cargo)))?;
        let (updated, added) = project::add_cargo_dependencies(&text, &project::TAURI_CRATES);
        if !added.is_empty() {
            files::replace(&cargo, updated.as_bytes())?;
        }
        report(&project::display(&cargo), &added);
    } else {
        println!(
            "! {} introuvable : ajoute les plugins {}",
            project::display(&cargo),
            project::TAURI_CRATES.map(|(name, _)| name).join(", ")
        );
    }

    let package = project.root.join("package.json");
    if package.is_file() {
        let mut json = Json::read(&package)?;
        let mut changes = Vec::new();
        // L'aide déjà déclarée par l'adresse du dépôt, avec une plage qui s'arrête avant la
        // version de l'outil : la plage la suit, là où l'app la déclare. Les plugins qu'ajoute
        // cette version de l'outil servent à l'aide de la même version.
        if let Some((section, value, change)) = raised_helper(&json) {
            json.set(&[section, project::HELPER_PACKAGE], Json::string(&value))?;
            changes.push(change);
        }
        let helper = project::helper_dependency();
        for (name, version) in
            iter::once((project::HELPER_PACKAGE, helper.as_str())).chain(project::TAURI_PACKAGES)
        {
            if project::npm_dependency(&json, name).is_none() {
                json.set(&["dependencies", name], Json::string(version))?;
                changes.push(name.to_owned());
            }
        }
        if !changes.is_empty() {
            json.write(&package)?;
        }
        report(&project::display(&package), &changes);
    } else {
        println!(
            "! {} introuvable : ajoute les paquets {} et {}",
            project::display(&package),
            project::HELPER_PACKAGE,
            project::TAURI_PACKAGES.map(|(name, _)| name).join(", ")
        );
    }
    Ok(identity)
}

/// L'aide `@drakbase/tauri` d'un `package.json`, quand elle est à réécrire : sa section, la valeur
/// à écrire, et ce qu'en dit le compte rendu. Sa plage monte quand elle s'arrête avant la version
/// de l'outil, et son adresse passe à celle du dépôt actuel quand c'est l'ancienne. Toute autre
/// plage reste telle quelle : celle qui demande une aide plus récente que l'outil (`init` ne ramène
/// jamais une dépendance en arrière), et celle qui ne se lit pas, que le contrôle final signale.
/// Une aide déclarée autrement (`file:`, `workspace:`, `link:`, une étiquette git) reste aussi.
fn raised_helper(package: &Json) -> Option<(&'static str, String, String)> {
    let name = project::HELPER_PACKAGE;
    let (section, declared) = project::npm_dependency(package, name)?;
    let helper = HelperRange::parse(declared.as_str()?)?;
    let older = helper.fit() == RangeFit::Older;
    let mut said = Vec::new();
    if older {
        said.push(format!(
            "monté de {} à {}",
            helper.range,
            project::client_range()
        ));
    }
    if helper.moved() {
        said.push(format!(
            "pris au dépôt public ({})",
            project::REPOSITORY_PATH
        ));
    }
    if said.is_empty() {
        return None;
    }
    let range = if older {
        project::client_range()
    } else {
        helper.range.to_owned()
    };
    Some((
        section,
        helper.with_range(&range),
        format!("{name} {}", said.join(", ")),
    ))
}

/// Configure l'app Swift ; rend son identifiant de bundle et son nom, s'ils sont écrits tels
/// quels.
fn configure_swift(
    project: &SwiftProject,
    app: &AppSlug,
    server: &str,
    keys: &Keys,
) -> Result<Identity> {
    let Some(path) = &project.plist else {
        println!(
            "! Aucun Info.plist dans le projet. Pour un projet Xcode dont l'Info.plist est généré, ajoute-en un (réglage « Info.plist File ») ; puis relance la commande, avec --plist <fichier> s'il le faut."
        );
        return Ok(Identity {
            bundle_id: None,
            name: None,
        });
    };
    let name = project::display(path);
    let mut value = plist::Value::from_file(path).with_context(|| format!("lecture de {name}"))?;
    let info = value
        .as_dictionary_mut()
        .with_context(|| format!("{name} : dictionnaire attendu"))?;
    let mut changes = Vec::new();

    // Les clés d'avant la 0.3 prennent leur nouveau nom, sans changer de valeur. Si les deux noms
    // sont là, le paquet ne lit que le nouveau : l'ancien est retiré.
    for key in project::INFO_KEYS {
        let Some(old) = info.remove(key.legacy) else {
            continue;
        };
        if info.contains_key(key.name) {
            changes.push(format!("{} retirée ({} l'emporte)", key.legacy, key.name));
        } else {
            info.insert(key.name.to_owned(), old);
            changes.push(format!("{} renommée {}", key.legacy, key.name));
        }
    }

    let feed = project::swift_feed(server, app);
    let feed_key = project::FEED_KEY.name;
    if info.get(feed_key).and_then(plist::Value::as_string) != Some(feed.as_str()) {
        info.insert(feed_key.to_owned(), plist::Value::String(feed));
        changes.push(format!("adresse de vérification ({feed_key})"));
    }

    let keys_key = project::PUBLIC_KEYS_KEY.name;
    // Une valeur qui n'est pas une liste de clés ne se remplace pas non plus.
    let existing: Option<Vec<&str>> = info.get(keys_key).map(|keys| {
        keys.as_array()
            .map(|items| items.iter().filter_map(plist::Value::as_string).collect())
            .unwrap_or_default()
    });
    if embed_keys(existing.as_deref(), keys, &name, keys_key)? {
        info.insert(
            keys_key.to_owned(),
            plist::Value::Array(
                wanted_keys(keys)
                    .into_iter()
                    .map(plist::Value::String)
                    .collect(),
            ),
        );
        changes.push(keys_change(keys, keys_key));
    }
    // Un réglage de Xcode, « $(PRODUCT_BUNDLE_IDENTIFIER) », ne dit rien ici.
    let literal = |key: &str| {
        info.get(key)
            .and_then(plist::Value::as_string)
            .filter(|text| !text.contains("$("))
            .map(str::to_owned)
    };
    let identity = Identity {
        bundle_id: literal("CFBundleIdentifier"),
        name: literal("CFBundleDisplayName").or_else(|| literal("CFBundleName")),
    };
    if !changes.is_empty() {
        let mut bytes = Vec::new();
        value
            .to_writer_xml(&mut bytes)
            .with_context(|| format!("écriture de {name}"))?;
        bytes.push(b'\n');
        files::replace(path, &bytes)?;
    }
    report(&name, &changes);
    Ok(identity)
}

/// Les clés publiques à embarquer : la principale, puis celle de secours.
fn wanted_keys(keys: &Keys) -> Vec<String> {
    iter::once(keys.main.clone())
        .chain(keys.backup.clone())
        .collect()
}

/// Faut-il écrire les clés ? La règle de tous les projets, Info.plist, `stack.json` ou
/// `drakbase.json` : jamais par-dessus d'autres clés. La première déjà écrite (`existing`) doit
/// être la clé principale. Vrai si rien n'est écrit, ou s'il manque la clé de secours.
fn embed_keys(existing: Option<&[&str]>, keys: &Keys, file: &str, field: &str) -> Result<bool> {
    let Some(current) = existing else {
        return Ok(true);
    };
    let first = current
        .first()
        .and_then(|key| project::key_id(key).ok())
        .map(|id| id.to_hex());
    if first.as_deref() != Some(keys.main_id.to_hex().as_str()) {
        bail!(
            "{file} embarque déjà d'autres clés ({field}, d'abord {}) : je ne les remplace pas. Changer de clé demande une version de transition.",
            first.unwrap_or_else(|| "une clé illisible".to_owned())
        );
    }
    Ok(current.len() < wanted_keys(keys).len())
}

fn keys_change(keys: &Keys, field: &str) -> String {
    match &keys.backup {
        Some(_) => format!("clés publiques ({field}), la principale puis celle de secours"),
        None => format!("clé publique principale ({field})"),
    }
}

/// Les clés déjà écrites dans un fichier JSON, `stack.json` ou `drakbase.json` ; `None` sans ce
/// champ.
fn json_keys<'a>(json: &'a Json, file: &str, field: &str) -> Result<Option<Vec<&'a str>>> {
    json.get(&[field])
        .map(|value| {
            let items = value
                .as_array()
                .with_context(|| format!("{file} : {field} doit être une liste"))?;
            Ok(items.iter().filter_map(Json::as_str).collect())
        })
        .transpose()
}

/// Le `stack.json` d'une stack : ses clés publiques, et un squelette s'il manque.
fn configure_stack(project: &StackProject, app: &AppSlug, keys: &Keys) -> Result<Identity> {
    let path = project.stack_json();
    let name = project::display(&path);
    let mut changes = Vec::new();
    let mut json = if path.exists() {
        Json::read(&path)?
    } else {
        let service = guess_service(project, app);
        changes.push(format!(
            "créé, avec le service surveillé « {service} » (à vérifier) et les plateformes {}",
            stack::platform_list(&DEFAULT_PLATFORMS)
        ));
        let platforms = DEFAULT_PLATFORMS
            .iter()
            .filter_map(|arch| am_protocol::stack::docker_platform(*arch))
            .map(Json::string)
            .collect();
        Json::Object(IndexMap::from([
            ("format".to_owned(), Json::Number(1.into())),
            ("service".to_owned(), Json::string(&service)),
            ("platforms".to_owned(), Json::Array(platforms)),
        ]))
    };
    if !matches!(json, Json::Object(_)) {
        bail!("{name} : un objet JSON est attendu");
    }
    let existing = json_keys(&json, &name, "public_keys")?;
    if embed_keys(existing.as_deref(), keys, &name, "public_keys")? {
        json.set(
            &["public_keys"],
            Json::Array(
                wanted_keys(keys)
                    .iter()
                    .map(|key| Json::string(key))
                    .collect(),
            ),
        )?;
        changes.push(keys_change(keys, "public_keys"));
    }
    if !changes.is_empty() {
        json.write(&path)?;
    }
    report(&name, &changes);
    let bytes = fs::read(&path).with_context(|| format!("lecture de {name}"))?;
    if let Err(error) = StackManifest::parse_source(&bytes) {
        println!("! {name} : {error} ({})", error.code());
    }
    if !project.env_example().is_file() {
        println!(
            "· pas de {} : la machine n'aura aucun réglage à changer (facultatif)",
            project::display(&project.env_example())
        );
    }
    Ok(Identity {
        bundle_id: None,
        name: None,
    })
}

/// Le service surveillé, deviné dans le compose : le seul service sans profil, sinon celui qui
/// porte le nom de l'app, sinon le premier construit sur place. Sans Docker Compose, le nom de
/// l'app.
fn guess_service(project: &StackProject, app: &AppSlug) -> String {
    let source = match project.compose_project(&app.to_string()).source() {
        Ok(source) => source,
        Err(error) => {
            let problem = crate::describe(&error);
            println!(
                "! service surveillé : {}, faute de lire le compose ({})",
                app,
                problem.lines().next().unwrap_or_default()
            );
            return app.to_string();
        }
    };
    let Some(services) = source.get("services").and_then(Value::as_object) else {
        return app.to_string();
    };
    let mut candidates: Vec<(&String, &Value)> = services
        .iter()
        .filter(|(_, service)| {
            service
                .get("profiles")
                .and_then(Value::as_array)
                .is_none_or(|profiles| profiles.is_empty())
        })
        .collect();
    candidates.sort_by_key(|(name, _)| *name);
    let chosen = match candidates.as_slice() {
        [(only, _)] => Some(*only),
        _ => candidates
            .iter()
            .find(|(name, _)| **name == app.to_string())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|(_, service)| service.get("build").is_some_and(|build| !build.is_null()))
            })
            .or_else(|| candidates.first())
            .map(|(name, _)| *name),
    };
    chosen
        .filter(|name| is_service_name(name))
        .cloned()
        .unwrap_or_else(|| app.to_string())
}

/// Le `drakbase.json` d'une app cli : l'app et ses clés publiques.
fn configure_cli(project: &CliProject, app: &AppSlug, keys: &Keys) -> Result<Identity> {
    let path = &project.file;
    let name = project::display(path);
    let mut changes = Vec::new();
    let mut json = if path.exists() {
        Json::read(path)?
    } else {
        changes.push("créé".to_owned());
        Json::Object(IndexMap::new())
    };
    if !matches!(json, Json::Object(_)) {
        bail!("{name} : un objet JSON est attendu");
    }
    match json.get(&["app"]).map(Json::as_str) {
        Some(Some(existing)) if existing != app.to_string() => bail!(
            "{name} est celui de l'app {existing}, pas de {app} : vérifie l'identifiant de l'app"
        ),
        Some(Some(_)) => {}
        Some(None) => bail!("{name} : app doit être un texte"),
        None => {
            json.set(&["app"], Json::string(&app.to_string()))?;
            changes.push(format!("app {app}"));
        }
    }
    let existing = json_keys(&json, &name, "public_keys")?;
    if embed_keys(existing.as_deref(), keys, &name, "public_keys")? {
        json.set(
            &["public_keys"],
            Json::Array(
                wanted_keys(keys)
                    .iter()
                    .map(|key| Json::string(key))
                    .collect(),
            ),
        )?;
        changes.push(keys_change(keys, "public_keys"));
    }
    if !changes.is_empty() {
        json.write(path)?;
    }
    report(&name, &changes);
    cli_project::CliManifest::read(path)?;
    Ok(Identity {
        bundle_id: None,
        name: None,
    })
}

fn stack_steps(step: &mut Steps, project: &StackProject) {
    step.next(&format!(
        "Vérifier dans {} le service surveillé (service), le retour arrière (rollback : safe ou manual) et les plateformes (platforms). Un service lancé avant chaque mise à jour, une sauvegarde par exemple, se déclare dans pre_update, avec le seul profil drakbase-hooks.",
        project::display(&project.stack_json())
    ));
    step.next(&format!(
        "Mettre les réglages de la machine dans {}, avec leur valeur par défaut, et les données dans des volumes nommés, ou des chemins absolus réglés par .env : le dossier de la stack change à chaque version. Les fichiers montés vont dans files/, en lecture seule.",
        project::display(&project.env_example())
    ));
}

fn cli_steps(step: &mut Steps) {
    step.next(&format!(
        "Embarquer les clés publiques dans l'exécutable : lire public_keys de {} à la compilation (include_str! par exemple), et vérifier chaque mise à jour avec elles.",
        cli_project::FILE
    ));
    step.next(
        "Construire un exécutable Linux par arch, de préférence statique (cargo build --release --target aarch64-unknown-linux-musl, puis x86_64-unknown-linux-musl).",
    );
}

fn tauri_steps(step: &mut Steps, project: &TauriProject, app: &AppSlug) {
    step.next("Installer les paquets ajoutés : npm install");
    step.next(&format!(
        "Enregistrer les plugins dans {}/src/main.rs (ou lib.rs), dans le Builder de Tauri :",
        project::display(&project.src_tauri)
    ));
    step.code(&[
        ".plugin(tauri_plugin_updater::Builder::new().build())",
        ".plugin(tauri_plugin_process::init())",
        ".plugin(tauri_plugin_store::Builder::new().build())",
        ".plugin(tauri_plugin_os::init())",
    ]);
    step.next("L'interface, avec l'aide (clients/tauri/README.md, section 4) :");
    step.code(&[
        "import { useUpdater } from \"@drakbase/tauri/react\";",
        "const updater = useUpdater(); // cherche une mise à jour au démarrage",
    ]);
    step.next(
        "Construire avec la clé de l'app, qui signe les archives ; son mot de passe vient du trousseau :",
    );
    step.code(&[&format!("drakbase exec {app} -- npx tauri build")]);
}

fn swift_steps(step: &mut Steps, project: &SwiftProject) {
    let from = format!("from: \"{}\"", project::CLIENT_VERSION);
    let product = project::SWIFT_PRODUCT;
    let legacy = project::LEGACY_SWIFT_PRODUCT;
    let package_lines = [
        format!(".package(url: \"{}\", {from}),", project::REPOSITORY_URL),
        format!(
            "et dans la cible de l'app : .product(name: \"{product}\", package: \"{}\"),",
            project::SWIFT_PACKAGE
        ),
    ];
    match &project.manifest {
        SwiftManifest::Package(path) => {
            let manifest = std::slice::from_ref(path);
            if project::any_file_contains(manifest, &format!("\"{product}\"")) {
                if project::legacy_swift_package(manifest).is_some() {
                    step.next(
                        "Passer le paquet au dépôt public, dans Package.swift : son adresse, et son nom dans .product(…) :",
                    );
                    step.code(&[&package_lines[0], &package_lines[1]]);
                } else {
                    step.next(&format!(
                        "La dépendance {product} est déjà dans Package.swift."
                    ));
                }
            } else if project::any_file_contains(manifest, legacy) {
                step.next(&format!(
                    "Renommer la dépendance {legacy} en {product} dans Package.swift, à partir de la {} :",
                    project::CLIENT_VERSION
                ));
                step.code(&[&package_lines[0], &package_lines[1]]);
            } else {
                step.next("Ajouter la dépendance dans Package.swift :");
                step.code(&[&package_lines[0], &package_lines[1]]);
            }
        }
        SwiftManifest::Xcode(path) => {
            let pbxproj = [path.join("project.pbxproj")];
            if project::any_file_contains(&pbxproj, &format!("productName = {product};")) {
                if project::legacy_swift_package(&pbxproj).is_some() {
                    step.next(&format!(
                        "Passer le paquet au dépôt public, dans Xcode : retirer le paquet {product}, puis l'ajouter par File › Add Package Dependencies…, adresse {}, version {} ou plus.",
                        project::REPOSITORY_URL,
                        project::CLIENT_VERSION
                    ));
                } else {
                    step.next(&format!(
                        "La dépendance {product} est déjà dans le projet Xcode."
                    ));
                }
            } else {
                let action = if project::any_file_contains(&pbxproj, legacy) {
                    format!("Remplacer le produit {legacy} par {product} dans Xcode")
                } else {
                    "Ajouter la dépendance dans Xcode".to_owned()
                };
                step.next(&format!(
                    "{action} : File › Add Package Dependencies…, adresse {}, version {} ou plus, produit {product}.",
                    project::REPOSITORY_URL,
                    project::CLIENT_VERSION
                ));
            }
        }
    }
    step.next("Le code (clients/swift/DrakBase/README.md, « Intégration ») :");
    step.code(&[
        &format!("import {product}"),
        "@main @MainActor enum Main {",
        "    static func main() { RelaunchGate.waitForPreviousInstance(); MonApp.main() }",
        "}",
        "let updater = Updater(); updater.presenter = UpdatePrompt(updater: updater); updater.start()",
        ".commands { UpdatesCommands(updater: updater) }, et UpdateSettingsSection(updater: updater) dans les réglages",
    ]);
    step.next(
        "Signer chaque build avec ton certificat de signature de code (codesign --sign \"<nom du certificat>\"), jamais ad hoc : une app signée ad hoc ne reçoit aucune mise à jour.",
    );
}
