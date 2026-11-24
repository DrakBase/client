//! `drakbase publish --app <app> --version <v> --channel stable|beta [--notes <fichier.md>]
//! [--tauri-config <chemin>] [--publish] [--force] [--prune] <artefacts…>` : envoie les artefacts
//! signés d'une version dans un brouillon, créé ou repris, puis la publie avec `--publish`
//! (docs/API.md, « Publication par l'outil »).
//!
//! Tout ce qui peut l'être est contrôlé avant le premier envoi : jeton et portées, version plus
//! haute que la dernière du canal, taille des fichiers, identifiant du bundle, signatures
//! vérifiées avec les clés de l'app. La version déclare les clés qu'elle embarque : celles de
//! l'Info.plist d'une app Swift, celle de la configuration d'une app Tauri (`tauri.rs`), les
//! `public_keys` du `stack.json` des archives d'une stack, ou celles du `drakbase.json` d'une app
//! `cli` (`cli_project.rs`).
//!
//! Après un échec, le brouillon reste sur le serveur, et la même commande reprend là où elle
//! s'était arrêtée : un artefact déjà arrivé, identique, ne repart pas. Identique veut dire le
//! même fichier, signé par la même clé pour la même version : un fichier signé de nouveau avec une
//! autre clé repart, sinon le serveur garderait l'ancienne signature. Un brouillon repris garde
//! ses notes tant que `--notes` n'en donne pas d'autres : sans cette option, l'outil n'envoie pas
//! de notes du tout. Avec `--prune`, le brouillon perd les artefacts envoyés avant qui ne font pas
//! partie de la commande, un fichier envoyé par erreur par exemple.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::publish::MAX_NOTES_CHARS;
use am_protocol::signature::{KeyId, PublicKey, Signature, SignatureError};
use am_protocol::{Channel, Os, Version};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

use crate::api::{self, Api, AppInfo, AppKind, Artifact, KeyStatus, NewRelease, Release, Upload};
use crate::artifact::{self, Designation, LocalArtifact};
use crate::cli_project::{self, CliManifest};
use crate::connection::{self, ConnectError, Connection, PUBLISH_SCOPE, WRITE_SCOPE, failure};
use crate::credentials::{Secrets, ServerUrl};
use crate::home::{AppSlug, HomeArgs};
use crate::tauri;

/// Morceaux lus pour le SHA-256 et la vérification des signatures.
const READ_BYTES: usize = 256 * 1024;

#[derive(clap::Args)]
pub struct Args {
    /// Artefacts : <os>/<arch>/<type>=<fichier>, par exemple darwin/universal/app=Nom-1.2.0.app.tar.gz,
    /// ou le fichier seul quand il suffit à déduire son emplacement. Leur signature <fichier>.sig
    /// est lue à côté, sauf pour un dmg
    #[arg(value_name = "ARTEFACT", required = true)]
    artifacts: Vec<String>,

    /// App de la version (slug), par exemple translate
    #[arg(long, value_name = "APP")]
    app: AppSlug,

    /// Version, publiée sous sa forme canonique (1.2 devient 1.2.0)
    #[arg(long, value_name = "VERSION")]
    version: String,

    /// Canal de la version : stable ou beta
    #[arg(long, value_name = "CANAL", value_parser = parse_channel)]
    channel: Channel,

    /// Notes de version, en Markdown ; sans cette option, un brouillon repris garde les siennes
    #[arg(long, value_name = "FICHIER.md")]
    notes: Option<PathBuf>,

    #[command(flatten)]
    tauri: TauriConfigArg,

    /// drakbase.json d'une app cli, ou son dossier : la version déclare les clés publiques qu'il
    /// donne [défaut : drakbase.json du dossier courant]
    #[arg(long = "drakbase-json", value_name = "CHEMIN")]
    drakbase_json: Option<PathBuf>,

    /// Publie la version une fois les artefacts envoyés ; sans cette option, elle reste en
    /// brouillon
    #[arg(long)]
    publish: bool,

    /// Accepte une version inférieure ou égale à la dernière du canal
    #[arg(long)]
    force: bool,

    /// Retire du brouillon les artefacts envoyés avant qui ne font pas partie de cette commande
    #[arg(long)]
    prune: bool,

    #[command(flatten)]
    home: HomeArgs,
}

/// `--tauri-config`, commun à `publish` et `release`.
#[derive(clap::Args)]
pub(crate) struct TauriConfigArg {
    /// Configuration d'une app Tauri, tauri.conf.json ou son dossier : la version déclare la clé
    /// publique qu'elle donne [défaut : src-tauri/tauri.conf.json ou tauri.conf.json du dossier
    /// courant]
    #[arg(long = "tauri-config", value_name = "CHEMIN")]
    pub(crate) path: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let version = Version::parse_release(&args.version).context("--version")?;
    let notes = args.notes.as_deref().map(read_notes).transpose()?;
    let designations = args
        .artifacts
        .iter()
        .map(|text| Designation::parse(text))
        .collect::<Result<Vec<_>>>()?;
    let artifacts = artifact::prepare(&designations, &args.app, &version)?;
    let secrets = Secrets::select(&home)?;
    let request = Request {
        app: &args.app,
        version: &version,
        typed_version: Some(args.version.trim()),
        channel: args.channel,
        notes: notes.as_deref(),
        tauri_config: args.tauri.path.as_deref(),
        drakbase_json: args.drakbase_json.as_deref(),
        publish: args.publish,
        force: args.force,
        prune: args.prune,
        draft_hint: None,
    };
    let session = Session::open(&secrets, &request)?;
    session.deliver(&request, &artifacts)?;
    Ok(ExitCode::SUCCESS)
}

/// `--channel` : `stable` ou `beta`.
pub(crate) fn parse_channel(value: &str) -> Result<Channel, String> {
    value
        .parse::<Channel>()
        .map_err(|_| "stable ou beta attendu".to_owned())
}

/// Lit les notes de version, en Markdown.
pub(crate) fn read_notes(path: &Path) -> Result<String> {
    let notes = fs::read_to_string(path).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData => {
            anyhow!("notes {} : le fichier n'est pas en UTF-8", path.display())
        }
        _ => anyhow::Error::new(error).context(format!("notes {}", path.display())),
    })?;
    let count = notes.chars().count();
    ensure!(
        count <= MAX_NOTES_CHARS,
        "notes {} trop longues : {count} caractères, {MAX_NOTES_CHARS} au plus",
        path.display()
    );
    Ok(notes)
}

/// Ce qu'il faut envoyer.
pub(crate) struct Request<'a> {
    pub(crate) app: &'a AppSlug,
    pub(crate) version: &'a Version,
    /// La version telle que tapée, pour montrer sa forme canonique.
    pub(crate) typed_version: Option<&'a str>,
    pub(crate) channel: Channel,
    pub(crate) notes: Option<&'a str>,
    /// `--tauri-config` : d'où lire la clé qu'embarque une version d'une app Tauri.
    pub(crate) tauri_config: Option<&'a Path>,
    /// `--drakbase-json` : d'où lire les clés qu'embarque une version d'une app `cli`.
    pub(crate) drakbase_json: Option<&'a Path>,
    pub(crate) publish: bool,
    pub(crate) force: bool,
    /// Retire du brouillon les artefacts qui ne font pas partie de la commande.
    pub(crate) prune: bool,
    /// Ce qu'il faut faire pour publier le brouillon, à la place de « la même commande avec
    /// --publish ».
    pub(crate) draft_hint: Option<&'a str>,
}

/// Où lire les clés qu'embarque une version.
enum KeySource {
    /// La configuration d'une app Tauri.
    Tauri,
    /// Le `drakbase.json` d'une app `cli`.
    Cli,
    /// Les artefacts eux-mêmes : Info.plist des archives macOS, `stack.json` des archives de
    /// stack.
    Artifacts,
}

/// Connexion au serveur, une fois le jeton, l'app et la version contrôlés.
pub(crate) struct Session {
    connection: Connection,
    app: AppInfo,
}

/// Une clé de l'app, donnée par le serveur.
struct ServerKey {
    public: PublicKey,
    status: KeyStatus,
}

impl Session {
    /// Ouvre la connexion enregistrée par `login`, puis contrôle le jeton, l'app et la version,
    /// avant tout envoi.
    pub(crate) fn open(secrets: &Secrets, request: &Request<'_>) -> Result<Self> {
        let connection = Connection::open(secrets).map_err(ConnectError::into_error)?;
        check_token(&connection, request)?;
        let app = connection.app(request.app)?;
        let session = Self { connection, app };
        session.print_summary(request);
        session.check_version(request)?;
        Ok(session)
    }

    fn print_summary(&self, request: &Request<'_>) {
        println!("Serveur : {}", self.connection.describe());
        println!(
            "App     : {} ({}), canal {}",
            self.app.slug,
            self.app.name,
            channel_name(request.channel)
        );
        if let Some(warning) = connection::expiry_warning(&self.connection.whoami.token) {
            eprintln!("attention : {warning}");
        }
    }

    /// Refuse une version qui n'est pas plus haute que la dernière du canal, sauf avec `--force`.
    fn check_version(&self, request: &Request<'_>) -> Result<()> {
        let version = request.version;
        let shown = match request.typed_version {
            Some(typed) if typed != version.to_string() => {
                format!("{version} (forme canonique de « {typed} »)")
            }
            _ => version.to_string(),
        };
        let name = channel_name(request.channel);
        let latest = self.app.latest.of(request.channel);
        match latest {
            Some(latest) => println!("Version : {shown}, dernière version {name} : {latest}"),
            None => println!("Version : {shown}, aucune version {name} publiée jusqu'ici"),
        }
        // Publiée sur l'autre canal, la version existe déjà : le serveur la refuserait.
        match request.channel {
            Channel::Stable if self.app.latest.of(Channel::Beta) == Some(version) => bail!(
                "la version {version} est déjà publiée en bêta : pour la passer en stable, change son canal dans l'administration, sans rien renvoyer (le canal ne fait pas partie de la signature)"
            ),
            Channel::Beta if self.app.latest.of(Channel::Stable) == Some(version) => bail!(
                "la version {version} est déjà publiée en stable : choisis une version plus haute"
            ),
            _ => {}
        }
        if let Some(latest) = latest
            && version <= latest
        {
            ensure!(
                request.force,
                "la version {version} n'est pas plus haute que la dernière version {name} publiée, {latest} : les apps déjà à jour ne la recevraient pas. Choisis une version plus haute, ou relance avec --force si c'est voulu."
            );
            eprintln!(
                "attention : --force, la version {version} n'est pas plus haute que la dernière version {name}, {latest}"
            );
        }
        if request.channel == Channel::Beta
            && let Some(stable) = self.app.latest.of(Channel::Stable)
            && version <= stable
        {
            eprintln!(
                "attention : la version stable {stable} est plus haute ; les testeurs de la bêta la garderont, sans recevoir {version}"
            );
        }
        Ok(())
    }

    /// Refuse tôt un fichier plus gros que ce qu'accepte le serveur, avant de le lire et de
    /// l'envoyer.
    pub(crate) fn check_size(&self, path: &Path, size: u64) -> Result<()> {
        let max = self.connection.whoami.max_artifact_bytes;
        ensure!(
            size <= max,
            "{} fait {}, plus que la taille maximale d'un artefact sur le serveur ({}) : réduis le fichier, ou relève AM_MAX_ARTIFACT_BYTES sur le serveur",
            path.display(),
            am_protocol::human_size(size),
            am_protocol::human_size(max)
        );
        Ok(())
    }

    /// L'identifiant du bundle doit être celui de l'app sur le serveur, quand elle en a un.
    pub(crate) fn check_bundle_id(&self, found: Option<&str>, origin: &Path) -> Result<()> {
        if let Some(expected) = &self.app.bundle_id {
            let found = found.unwrap_or("aucun");
            ensure!(
                found == expected,
                "{} a l'identifiant {found}, alors que l'app {} attend {expected} : ce n'est pas le bundle de cette app",
                origin.display(),
                self.app.slug
            );
        }
        Ok(())
    }

    /// Adresse publique du serveur, celle que voient les machines qui installent : elle peut
    /// différer de celle par laquelle l'outil publie (un tunnel, une adresse interne).
    pub(crate) fn public_server(&self) -> Result<ServerUrl> {
        let url = &self.connection.whoami.public_url;
        ServerUrl::parse(url).with_context(|| {
            format!(
                "le serveur annonce une adresse publique que l'outil ne peut pas employer (« {url} », son AM_PUBLIC_URL)"
            )
        })
    }

    /// L'app, telle que la décrit le serveur.
    pub(crate) fn app(&self) -> &AppInfo {
        &self.app
    }

    /// Contrôle les artefacts, crée ou reprend le brouillon, envoie ce qui manque, puis publie si
    /// demandé.
    pub(crate) fn deliver(&self, request: &Request<'_>, artifacts: &[LocalArtifact]) -> Result<()> {
        self.deliver_with(request, artifacts, |_| Ok(()))
    }

    /// Comme [`Session::deliver`], avec `before_artifacts` entre la création du brouillon et
    /// l'envoi des artefacts : les images d'une stack doivent être sur le serveur avant ses
    /// archives.
    pub(crate) fn deliver_with(
        &self,
        request: &Request<'_>,
        artifacts: &[LocalArtifact],
        before_artifacts: impl FnOnce(&Api) -> Result<()>,
    ) -> Result<()> {
        for artifact in artifacts {
            self.check_size(&artifact.path, artifact.size)?;
        }
        let checked = self.check_artifacts(request, artifacts)?;
        let embedded = match self.key_source(request)? {
            KeySource::Tauri => self.tauri_key(request, artifacts)?.map(|key| vec![key]),
            KeySource::Cli => self.cli_keys(request)?,
            KeySource::Artifacts => {
                let embedded = embedded_keys(artifacts)?;
                if let Some(embedded) = &embedded {
                    println!(
                        "Clés embarquées par cette version : {}",
                        embedded
                            .iter()
                            .map(|key| key.to_hex())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                embedded
            }
        };
        if let Some(embedded) = &embedded
            && let Some(signing) = checked.iter().find_map(|check| check.key)
            && !embedded.contains(&signing)
        {
            eprintln!(
                "attention : cette version n'embarque pas la clé {signing} qui la signe. Ses utilisateurs ne pourront vérifier une mise à jour suivante que si elle est signée par l'une des clés embarquées."
            );
        }

        let version = request.version;
        let new_release = NewRelease {
            version: version.to_string(),
            channel: Some(request.channel),
            // Sans --notes, rien n'est envoyé : un brouillon repris garde ses notes.
            notes_md: request.notes.map(str::to_owned),
            trusted_key_ids: embedded
                .as_ref()
                .map(|keys| keys.iter().map(|key| key.to_hex()).collect()),
        };
        let (release, created) = self.connection.api
            .create_release(request.app, &new_release)
            .map_err(|error| {
                let conflict = error.code() == Some("conflict");
                let error = failure(error, format!("création du brouillon {version}"));
                if conflict {
                    crate::with_advice(
                        error,
                        "La version existe déjà sur le serveur, hors brouillon : choisis une version plus haute.",
                    )
                } else {
                    error
                }
            })?;
        println!();
        // Le serveur ne rend ici qu'un brouillon : une version publiée ou retirée donne 409.
        if created {
            println!("Brouillon {version} créé sur le serveur.");
        } else {
            println!("Brouillon {version} repris : il était déjà sur le serveur.");
        }
        before_artifacts(&self.connection.api)
            .and_then(|()| self.fill(request, artifacts, &checked, &release))
            .map_err(|error| {
                crate::with_advice(
                    error,
                    &format!(
                        "Le brouillon {version} reste sur le serveur, avec les artefacts déjà arrivés. Relance la même commande pour reprendre, une fois la cause réglée : ce qui est déjà arrivé ne repartira pas."
                    ),
                )
            })
    }

    /// La clé embarquée se lit dans la configuration Tauri pour une app Tauri, dans
    /// `drakbase.json` pour une app `cli`, et ailleurs dans les artefacts eux-mêmes.
    fn key_source(&self, request: &Request<'_>) -> Result<KeySource> {
        let kind = self.app.kind;
        ensure!(
            kind == AppKind::Tauri || request.tauri_config.is_none(),
            "--tauri-config ne sert qu'aux apps Tauri : {} est {}",
            self.app.slug,
            api::described(kind)
        );
        ensure!(
            kind == AppKind::Cli || request.drakbase_json.is_none(),
            "--drakbase-json ne sert qu'aux apps cli : {} est {}",
            self.app.slug,
            api::described(kind)
        );
        Ok(match kind {
            AppKind::Tauri => KeySource::Tauri,
            AppKind::Cli => KeySource::Cli,
            AppKind::Swift | AppKind::Stack => KeySource::Artifacts,
        })
    }

    /// Clés qu'embarque la version d'une app `cli`, d'après son `drakbase.json`. Sans lui, la
    /// version ne peut que rester en brouillon, comme une app Tauri sans configuration.
    fn cli_keys(&self, request: &Request<'_>) -> Result<Option<Vec<KeyId>>> {
        let Some(path) = cli_project::locate(request.drakbase_json)? else {
            ensure!(
                !request.publish,
                "{} est une app cli, et le dossier courant n'a pas de {} : l'outil ne sait pas quelles clés publiques la version embarque. Publiée sans elles, la version bloquerait ses utilisateurs au prochain changement de clé. Lance la commande depuis le dossier du projet, ou donne le fichier avec --drakbase-json.",
                self.app.slug,
                cli_project::FILE
            );
            eprintln!(
                "attention : pas de {} dans le dossier courant, et pas de --drakbase-json. Avant de publier ce brouillon, coche dans l'administration les clés qu'il embarque.",
                cli_project::FILE
            );
            return Ok(None);
        };
        let manifest = CliManifest::read(&path)?;
        if let Some(app) = &manifest.app {
            ensure!(
                *app == self.app.slug,
                "{} est celui de l'app {app}, pas de {}",
                path.display(),
                self.app.slug
            );
        }
        let keys = manifest.key_ids();
        println!(
            "Clés embarquées par cette version : {} (d'après {})",
            keys.iter()
                .map(|key| key.to_hex())
                .collect::<Vec<_>>()
                .join(", "),
            path.display()
        );
        Ok(Some(keys))
    }

    /// Clé qu'embarque la version d'une app Tauri, d'après sa configuration. Sans configuration
    /// trouvée, la version ne peut que rester en brouillon : sa clé se coche alors dans
    /// l'administration, avant de la publier.
    fn tauri_key(
        &self,
        request: &Request<'_>,
        artifacts: &[LocalArtifact],
    ) -> Result<Option<KeyId>> {
        let Some(config) = tauri::find_config(request.tauri_config)? else {
            ensure!(
                !request.publish,
                "{} est une app Tauri, et le dossier courant n'a ni src-tauri/tauri.conf.json ni tauri.conf.json : l'outil ne sait pas quelle clé publique la version embarque. Publiée sans elle, la version bloquerait ses utilisateurs au prochain changement de clé. Lance la commande depuis le dossier du projet, ou donne sa configuration avec --tauri-config.",
                self.app.slug
            );
            eprintln!(
                "attention : ni src-tauri/tauri.conf.json ni tauri.conf.json dans le dossier courant, et pas de --tauri-config. Avant de publier ce brouillon, coche dans l'administration la clé qu'il embarque."
            );
            return Ok(None);
        };
        let oses: Vec<Os> = artifacts.iter().map(|artifact| artifact.slot.os).collect();
        let (key, origin) = tauri::embedded_key(&config, &oses)?;
        println!(
            "Clé embarquée par cette version : {key} (d'après {})",
            origin.display()
        );
        if !self
            .app
            .keys
            .iter()
            .any(|known| known.key_id.eq_ignore_ascii_case(&key.to_hex()))
        {
            eprintln!(
                "attention : la clé embarquée {key} n'est pas déclarée sur le serveur pour l'app {} : ses utilisateurs n'accepteront que des mises à jour signées par elle. Déclare-la dans l'administration avant de signer avec.",
                self.app.slug
            );
        }
        Ok(Some(key))
    }

    /// Clés de l'app déclarées sur le serveur.
    fn keys(&self) -> Result<Vec<ServerKey>> {
        ensure!(
            !self.app.keys.is_empty(),
            "l'app {} n'a aucune clé déclarée sur le serveur : déclare d'abord sa clé publique principale dans l'administration, comme clé active",
            self.app.slug
        );
        self.app
            .keys
            .iter()
            .map(|key| {
                Ok(ServerKey {
                    public: PublicKey::from_tauri(&key.public_key).with_context(|| {
                        format!(
                            "clé {} de l'app, donnée par le serveur, illisible",
                            key.key_id
                        )
                    })?,
                    status: key.status,
                })
            })
            .collect()
    }

    /// Identifiants, SHA-256 et signatures, artefact par artefact, avec un compte rendu.
    fn check_artifacts(
        &self,
        request: &Request<'_>,
        artifacts: &[LocalArtifact],
    ) -> Result<Vec<Checked>> {
        let keys = self.keys()?;
        println!();
        println!("Artefacts :");
        let mut checked: Vec<Checked> = Vec::new();
        let mut first_key: Option<(KeyId, &Path)> = None;
        for artifact in artifacts {
            if let Some(contents) = &artifact.contents {
                self.check_bundle_id(contents.info.identifier.as_deref(), &artifact.path)?;
            }
            let check = hash_and_verify(artifact, &keys, request.version, &self.app.slug)?;
            if let Some(key) = check.key {
                match first_key {
                    Some((first, other)) if first != key => bail!(
                        "les artefacts d'une version sont tous signés par la même clé : {} l'est par {first}, {} par {key}",
                        other.display(),
                        artifact.path.display()
                    ),
                    Some(_) => {}
                    None => first_key = Some((key, &artifact.path)),
                }
            }
            println!("✓ {}", describe_artifact(artifact, &check, &keys));
            checked.push(check);
        }
        Ok(checked)
    }

    /// Envoie au brouillon ce qui lui manque, puis publie si demandé. Créé ou repris, le brouillon
    /// a déjà le canal, les notes et les clés de la commande : sa création les lui donne
    /// (docs/API.md, « Brouillon existant »).
    fn fill(
        &self,
        request: &Request<'_>,
        artifacts: &[LocalArtifact],
        checked: &[Checked],
        release: &Release,
    ) -> Result<()> {
        let version = request.version;
        for (artifact, check) in artifacts.iter().zip(checked) {
            let slot = artifact.slot;
            let existing = release.artifacts.iter().find(|existing| {
                (existing.os, existing.arch, existing.bundle_type)
                    == (slot.os, slot.arch, slot.bundle_type)
            });
            if existing
                .is_some_and(|existing| same_artifact(existing, artifact.size, check, version))
            {
                println!("· {slot} : déjà sur le serveur, identique, pas renvoyé");
                continue;
            }
            let upload = Upload {
                os: slot.os,
                arch: slot.arch,
                bundle_type: slot.bundle_type,
                path: &artifact.path,
                size: artifact.size,
                file_name: &artifact.upload_name,
                signature: artifact.signature.as_deref(),
                sha256: &check.sha256,
            };
            let uploaded = self
                .connection
                .api
                .upload(request.app, version, &upload)
                .map_err(|error| {
                    failure(error, format!("envoi de {slot} ({})", artifact.file_name))
                })?;
            ensure!(
                uploaded.size == artifact.size
                    && uploaded.sha256.eq_ignore_ascii_case(&check.sha256),
                "envoi de {slot} : le serveur a enregistré un autre fichier (taille ou SHA-256 différents) ; relance la commande"
            );
            println!(
                "✓ {slot} : envoyé ({}){}",
                am_protocol::human_size(artifact.size),
                if existing.is_some() {
                    ", à la place de celui du brouillon"
                } else {
                    ""
                }
            );
        }
        let others: Vec<&Artifact> = release
            .artifacts
            .iter()
            .filter(|existing| {
                !artifacts.iter().any(|artifact| {
                    (existing.os, existing.arch, existing.bundle_type)
                        == (
                            artifact.slot.os,
                            artifact.slot.arch,
                            artifact.slot.bundle_type,
                        )
                })
            })
            .collect();
        let slot_of = |existing: &Artifact| {
            format!("{}/{}/{}", existing.os, existing.arch, existing.bundle_type)
        };
        if request.prune {
            for existing in &others {
                let slot = slot_of(existing);
                self.connection
                    .api
                    .delete_artifact(request.app, &existing.id)
                    .map_err(|error| failure(error, format!("retrait de {slot} du brouillon")))?;
                println!("✓ {slot} : retiré du brouillon");
            }
        } else if !others.is_empty() {
            println!(
                "  Le brouillon contient aussi, envoyés avant : {}. Relance avec --prune pour les en retirer.",
                others
                    .iter()
                    .map(|existing| slot_of(existing))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        println!();
        if !request.publish {
            match request.draft_hint {
                Some(hint) => println!("Brouillon {version} prêt. Pour le publier : {hint}"),
                None => println!(
                    "Brouillon {version} prêt. Pour le publier : la même commande avec --publish (rien ne sera renvoyé), ou l'administration."
                ),
            }
            return Ok(());
        }
        let has_update = artifacts
            .iter()
            .map(|artifact| artifact.slot.bundle_type)
            .chain(
                release
                    .artifacts
                    .iter()
                    .filter(|_| !request.prune)
                    .map(|existing| existing.bundle_type),
            )
            .any(|bundle_type| bundle_type.is_update_payload());
        ensure!(
            has_update,
            "il faut au moins un artefact de mise à jour (autre qu'un dmg) pour publier la version {version}"
        );
        let published = self
            .connection
            .api
            .publish(request.app, version)
            .map_err(|error| failure(error, format!("publication de la version {version}")))?;
        println!(
            "✓ Version {version} publiée sur le canal {}.",
            channel_name(published.channel)
        );
        for artifact in &published.artifacts {
            println!(
                "  {}/{}/{} : {}",
                artifact.os, artifact.arch, artifact.bundle_type, artifact.url
            );
        }
        Ok(())
    }
}

/// Le jeton doit pouvoir faire ce qui est demandé, pour cette app.
fn check_token(connection: &Connection, request: &Request<'_>) -> Result<()> {
    let token = &connection.whoami.token;
    ensure!(
        token.has_scope(WRITE_SCOPE),
        "le jeton « {} » n'a pas la portée {WRITE_SCOPE}, nécessaire pour envoyer une version : crée un jeton avec cette portée, puis relance « drakbase login »",
        token.name
    );
    if request.publish {
        ensure!(
            token.has_scope(PUBLISH_SCOPE),
            "le jeton « {} » n'a pas la portée {PUBLISH_SCOPE} : relance sans --publish pour laisser la version en brouillon, à publier depuis l'administration, ou crée un jeton avec cette portée",
            token.name
        );
    }
    connection.require_app(request.app)
}

/// Résultat des contrôles d'un artefact.
struct Checked {
    /// SHA-256 en hexadécimal minuscule.
    sha256: String,
    /// La clé de l'app qui le signe ; aucune pour un dmg.
    key: Option<KeyId>,
}

/// L'artefact du brouillon est-il celui de la commande, de cette taille ? Le même fichier (taille
/// et SHA-256), et la même signature : la clé qui signe et la version signée, que le serveur garde
/// avec lui. Un dmg n'a ni l'une ni l'autre, des deux côtés.
fn same_artifact(existing: &Artifact, size: u64, check: &Checked, version: &Version) -> bool {
    let same_file = existing.size == size && existing.sha256.eq_ignore_ascii_case(&check.sha256);
    let same_signature = match check.key {
        Some(key) => {
            existing
                .signed_key_id
                .as_deref()
                .is_some_and(|signed| signed.eq_ignore_ascii_case(&key.to_hex()))
                && existing.signed_version.as_deref() == Some(version.to_string().as_str())
        }
        None => existing.signed_key_id.is_none() && existing.signed_version.is_none(),
    };
    same_file && same_signature
}

/// Calcule le SHA-256 du fichier et, s'il est signé, vérifie sa signature avec la clé de l'app
/// qu'elle désigne, dans l'ordre de docs/PROTOCOLE.md, en une seule lecture.
fn hash_and_verify(
    artifact: &LocalArtifact,
    keys: &[ServerKey],
    version: &Version,
    app: &str,
) -> Result<Checked> {
    let path = &artifact.path;
    let mut file = File::open(path).with_context(|| format!("ouverture de {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; READ_BYTES];
    let Some(text) = &artifact.signature else {
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("lecture de {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        return Ok(Checked {
            sha256: hex::encode(hasher.finalize()),
            key: None,
        });
    };

    let signature = Signature::from_tauri(text)
        .with_context(|| format!("{}.sig : signature illisible", path.display()))?;
    let Some(key) = keys
        .iter()
        .find(|key| key.public.key_id() == signature.key_id())
    else {
        bail!(
            "{} est signé par la clé {}, que le serveur ne connaît pas pour l'app {app} (clés déclarées : {}). Signe-le avec la clé active de l'app, ou déclare cette clé dans l'administration.",
            path.display(),
            signature.key_id(),
            describe_keys(keys)
        );
    };
    ensure!(
        key.status != KeyStatus::Retired,
        "{} est signé par la clé {}, retirée de l'app {app} : signe-le avec la clé active",
        path.display(),
        signature.key_id()
    );
    let mut verifier = match key.public.verify_stream(&signature) {
        Ok(verifier) => verifier,
        Err(SignatureError::Legacy) => bail!(
            "{} : signature au format historique, non préhaché, que le serveur refuse ; signe de nouveau le fichier avec « drakbase sign »",
            path.display()
        ),
        Err(other) => {
            return Err(other).with_context(|| format!("signature de {}", path.display()));
        }
    };
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("lecture de {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        verifier.update(&buffer[..read]);
    }
    let verified = verifier.finalize().map_err(|_| {
        anyhow!(
            "{} : signature invalide. Le fichier a changé depuis sa signature, ou sa .sig est celle d'un autre fichier : signe-le de nouveau.",
            path.display()
        )
    })?;
    // La version n'est lue qu'à présent : avant, le commentaire de confiance n'était pas
    // authentifié.
    match verified.require_version(version) {
        Ok(()) => {}
        Err(SignatureError::MissingVersion) => bail!(
            "{} : la signature ne contient pas de version, que les apps exigent ; signe de nouveau le fichier avec « drakbase sign … --version {version} »",
            path.display()
        ),
        Err(SignatureError::VersionMismatch { signed, .. }) => bail!(
            "{} est signé pour la version {signed}, pas {version} : signe-le de nouveau avec --version {version}",
            path.display()
        ),
        Err(other) => return Err(other.into()),
    }
    Ok(Checked {
        sha256: hex::encode(hasher.finalize()),
        key: Some(key.public.key_id()),
    })
}

fn describe_keys(keys: &[ServerKey]) -> String {
    keys.iter()
        .map(|key| {
            format!(
                "{} {}",
                key.public.key_id(),
                api::key_status_label(key.status)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// « darwin/universal/app : Demo-1.2.0.app.tar.gz (7,3 Mio), fr.example.Demo 1.2, arm64 et x86_64,
/// signé par la clé … (active) ».
fn describe_artifact(artifact: &LocalArtifact, check: &Checked, keys: &[ServerKey]) -> String {
    let mut text = format!(
        "{} : {} ({})",
        artifact.slot,
        artifact.file_name,
        am_protocol::human_size(artifact.size)
    );
    if let Some(contents) = &artifact.contents {
        let info = &contents.info;
        text.push_str(&format!(
            ", {} {}, {}",
            info.identifier.as_deref().unwrap_or("sans identifiant"),
            info.raw_version,
            contents.cpus
        ));
    }
    if let Some(stack) = &artifact.stack {
        let images: Vec<&str> = stack.images.keys().map(String::as_str).collect();
        text.push_str(&format!(
            ", service {}, image(s) {}",
            stack.service,
            images.join(", ")
        ));
    }
    match check.key {
        Some(key) => {
            let status = keys
                .iter()
                .find(|known| known.public.key_id() == key)
                .map_or("", |known| api::key_status_label(known.status));
            text.push_str(&format!(", signé par la clé {key} ({status})"));
        }
        None => text.push_str(", sans signature (première installation)"),
    }
    if artifact.upload_name != artifact.file_name {
        text.push_str(&format!(", envoyé sous le nom {}", artifact.upload_name));
    }
    text
}

/// Clés que la version embarque, pour `trusted_key_ids` : d'après l'Info.plist de ses archives
/// macOS (`DrakBasePublicKeys`), ou les `public_keys` du `stack.json` de ses archives de stack.
/// `None` sans de telles archives ou sans ces clés.
fn embedded_keys(artifacts: &[LocalArtifact]) -> Result<Option<Vec<KeyId>>> {
    let mut first: Option<(&LocalArtifact, Vec<KeyId>)> = None;
    for artifact in artifacts {
        let (keys, field) = match (&artifact.contents, &artifact.stack) {
            (Some(contents), _) => (
                contents
                    .info
                    .embedded_key_ids()
                    .with_context(|| artifact.path.display().to_string())?,
                "DrakBasePublicKeys",
            ),
            (None, Some(stack)) => (stack.trusted_key_ids(), "public_keys de stack.json"),
            (None, None) => continue,
        };
        match &first {
            None => first = Some((artifact, keys)),
            Some((other, known)) => {
                let mut left = known.iter().map(|key| key.to_hex()).collect::<Vec<_>>();
                let mut right = keys.iter().map(|key| key.to_hex()).collect::<Vec<_>>();
                left.sort();
                right.sort();
                ensure!(
                    left == right,
                    "{} et {} n'embarquent pas les mêmes clés ({field})",
                    other.path.display(),
                    artifact.path.display()
                );
            }
        }
    }
    Ok(first.map(|(_, keys)| keys).filter(|keys| !keys.is_empty()))
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Beta => "bêta",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use am_protocol::{Arch, BundleType};

    /// Un artefact du brouillon, de 10 octets, tel que le rend le serveur.
    fn on_server(signed_key_id: Option<&str>, signed_version: Option<&str>) -> Artifact {
        Artifact {
            id: "01ESSAI0000000000000000000".to_owned(),
            os: Os::Darwin,
            arch: Arch::Aarch64,
            bundle_type: BundleType::App,
            filename: "Demo-1.2.0.app.tar.gz".to_owned(),
            size: 10,
            sha256: "AB".repeat(32),
            signed_key_id: signed_key_id.map(str::to_owned),
            signed_version: signed_version.map(str::to_owned),
            min_os_version: None,
            url: "https://maj.example.fr/dl/x".to_owned(),
            created_at: "2026-10-25T12:00:00Z".to_owned(),
        }
    }

    #[test]
    fn a_draft_artifact_is_kept_only_with_the_same_file_and_the_same_signature() {
        let version = Version::parse_release("1.2.0").unwrap();
        let key = KeyId::from_bytes([1, 2, 3, 4, 5, 6, 7, 0xab]);
        let signed = Checked {
            sha256: "ab".repeat(32),
            key: Some(key),
        };
        let unsigned = Checked {
            sha256: "ab".repeat(32),
            key: None,
        };
        let same =
            |existing: Artifact, check: &Checked| same_artifact(&existing, 10, check, &version);

        // La casse de l'identifiant et du SHA-256 ne compte pas.
        assert!(same(
            on_server(Some("AB07060504030201"), Some("1.2.0")),
            &signed
        ));
        assert!(same(
            on_server(Some("ab07060504030201"), Some("1.2.0")),
            &signed
        ));
        // Une autre clé, une autre version signée, ou aucune des deux.
        assert!(!same(
            on_server(Some("0102030405060708"), Some("1.2.0")),
            &signed
        ));
        assert!(!same(
            on_server(Some("AB07060504030201"), Some("1.1.9")),
            &signed
        ));
        assert!(!same(on_server(Some("AB07060504030201"), None), &signed));
        assert!(!same(on_server(None, None), &signed));
        // Un autre fichier.
        assert!(!same_artifact(
            &on_server(Some("AB07060504030201"), Some("1.2.0")),
            11,
            &signed,
            &version
        ));
        let other_file = Checked {
            sha256: "cd".repeat(32),
            key: Some(key),
        };
        assert!(!same(
            on_server(Some("AB07060504030201"), Some("1.2.0")),
            &other_file
        ));
        // Un dmg n'est signé d'aucun côté.
        assert!(same(on_server(None, None), &unsigned));
        assert!(!same(
            on_server(Some("AB07060504030201"), Some("1.2.0")),
            &unsigned
        ));
    }
}
