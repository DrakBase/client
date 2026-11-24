//! `drakbase release --app <app> --stack [<dossier>] --version <v> --channel stable|beta
//! [--notes <fichier.md>] [--publish] [--force] [--prune] [--output <dossier>] [--key <fichier>]` :
//! publie une version d'une stack Docker d'un coup (docs/PLAN.md, « Stacks Docker ») :
//!
//! 1. construction de chaque image par `docker buildx build`, pour les plateformes de
//!    `stack.json`, au format OCI et sans attestations ;
//! 2. recompression des couches en zstd -19, réécriture des manifestes et des index ;
//! 3. création ou reprise du brouillon, envoi des couches qui manquent au serveur, puis des
//!    manifestes, puis des index ;
//! 4. compose publié, contrôlé comme le fera l'agent ;
//! 5. `stack.json` publié et archive reproductible, signée et envoyée une fois par arch ;
//! 6. publication avec `--publish`, et la commande d'installation à transmettre.
//!
//! Le serveur et le compose sont consultés avant la construction : un jeton refusé, une version
//! trop basse ou une règle de stack enfreinte arrêtent tout avant de construire. Une publication
//! interrompue reprend avec la même commande : le serveur garde ce qu'il a reçu, et les couches
//! qui n'ont pas changé gardent leur empreinte.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::stack::Digest;
use am_protocol::stack::docker_platform;
use am_protocol::{BundleType, Channel, Os, Version};
use anyhow::{Context, Result, bail};

use super::docker::{self, Tools};
use super::images::{self, PreparedImage};
use super::{StackProject, archive, archive_name, examine, install_commands};
use crate::api::{self, AppKind};
use crate::artifact::{self, Designation, Slot};
use crate::credentials::Secrets;
use crate::files;
use crate::home::{AppSlug, Home};
use crate::project;
use crate::publish::{Request, Session};
use crate::sign;

/// Ce que demande `release --stack`.
pub struct Options<'a> {
    pub app: &'a AppSlug,
    pub dir: &'a Path,
    pub version: &'a Version,
    /// La version telle que tapée.
    pub typed_version: &'a str,
    pub channel: Channel,
    pub notes: Option<&'a str>,
    pub publish: bool,
    pub force: bool,
    pub prune: bool,
    /// Dossier des archives et de leurs signatures [défaut : `dist/` à côté de la stack].
    pub output: Option<&'a Path>,
    pub key: Option<&'a Path>,
}

pub fn release(secrets: &Secrets, home: &Home, options: &Options<'_>) -> Result<()> {
    let app = options.app;
    let version = options.version;
    let project = StackProject::at(options.dir)?;
    let manifest = project.manifest()?;
    println!(
        "Stack   : {} (service {}, plateformes {})",
        project::display(&project.dir),
        manifest.service,
        super::platform_list(&manifest.platforms)
    );

    let output = options
        .output
        .map_or_else(|| project.default_output(), Path::to_path_buf);
    let archives: Vec<(am_protocol::Arch, PathBuf)> = manifest
        .platforms
        .iter()
        .map(|arch| (*arch, output.join(archive_name(app, version, *arch))))
        .collect();
    let draft_hint = format!(
        "drakbase publish --app {app} --version {version} --channel {} --publish {} (rien ne sera reconstruit ni renvoyé), ou l'administration.",
        options.channel,
        archives
            .iter()
            .map(|(_, path)| project::display(path))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let request = Request {
        app,
        version,
        typed_version: Some(options.typed_version),
        channel: options.channel,
        notes: options.notes,
        tauri_config: None,
        drakbase_json: None,
        publish: options.publish,
        force: options.force,
        prune: options.prune,
        draft_hint: Some(&draft_hint),
    };
    let session = Session::open(secrets, &request)?;
    let kind = session.app().kind;
    if kind != AppKind::Stack {
        bail!(
            "l'app {app} est {} sur le serveur, pas une stack Docker : vérifie l'identifiant de l'app",
            api::described(kind)
        );
    }
    // Les machines tirent les images par l'adresse publique du serveur, qui peut différer de
    // celle par laquelle l'outil publie (un tunnel, une adresse interne).
    let public_server = session.public_server()?;
    let host = public_server.image_host();
    println!("Docker  : {}", Tools::detect().require()?);

    // Les règles du compose, avec des empreintes provisoires, avant de construire.
    println!();
    println!("Compose :");
    let first = examine(&project, &manifest, app, host, None)?;
    for image in &first.plan.images {
        println!(
            "· image {} : services {} (construite par {})",
            image.name,
            image.services().join(", "),
            image.built_by.join(", ")
        );
    }
    for note in &first.plan.notes {
        println!("! {note}");
    }

    // 1 et 2 : construction, puis recompression.
    let work = tempfile::Builder::new()
        .prefix("drakbase-release-")
        .tempdir()
        .context("dossier de travail")?;
    let platforms: Vec<&str> = manifest
        .platforms
        .iter()
        .filter_map(|arch| docker_platform(*arch))
        .collect();
    let mut prepared: Vec<PreparedImage> = Vec::new();
    for image in &first.plan.images {
        println!();
        println!(
            "Construction de l'image {} ({}) :",
            image.name,
            platforms.join(", ")
        );
        let oci_dir = work.path().join("oci").join(&image.name);
        docker::build(&image.build, &platforms, &oci_dir)?;
        println!("Recompression des couches de l'image {} :", image.name);
        prepared.push(images::prepare(
            &image.name,
            &oci_dir,
            &manifest.platforms,
            &work.path().join("blobs"),
        )?);
    }
    let digests: BTreeMap<String, Digest> = prepared
        .iter()
        .map(|image| (image.name.clone(), image.index.digest.clone()))
        .collect();

    // 4 : compose publié et contrôlé.
    println!();
    let examination = examine(&project, &manifest, app, host, Some(&digests))?;
    println!("✓ compose publié conforme aux règles des stacks");
    let report = &examination.report;
    if report.permissions.is_empty() {
        println!("  La stack ne demande rien de particulier à la machine.");
    } else {
        println!("  Ce que la stack demande à la machine, que l'agent fera accepter :");
        for permission in &report.permissions {
            println!("  · {permission}");
        }
    }
    for warning in &report.warnings {
        eprintln!("attention : {warning}");
    }

    // 5 : archive et signatures, une par arch.
    let bytes = archive(
        &project,
        &manifest,
        app,
        version,
        digests,
        &examination.published,
    )?;
    fs::create_dir_all(&output).with_context(|| format!("création de {}", output.display()))?;
    for (_, path) in &archives {
        files::replace(path, &bytes)?;
    }
    println!(
        "✓ Archives : {} ({} chacune)",
        archives
            .iter()
            .map(|(_, path)| project::display(path))
            .collect::<Vec<_>>()
            .join(", "),
        am_protocol::human_size(bytes.len() as u64)
    );
    let paths: Vec<&Path> = archives.iter().map(|(_, path)| path.as_path()).collect();
    let signed = sign::sign_files(&paths, app, version, options.key, home, secrets)?;
    for signature in &signed {
        println!(
            "✓ Signature : {}, clé {}",
            project::display(&signature.sig_path),
            sign::describe_key(signature, app)
        );
    }
    if let Some(first) = signed.first() {
        sign::warn_if_foreign(first, app, home);
    }

    // 3, puis l'envoi des archives : le brouillon, les images, enfin les archives.
    let designations = archives
        .iter()
        .map(|(arch, path)| {
            Ok(Designation::at(
                Slot::new(Os::Linux, *arch, BundleType::Stack)?,
                path.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let artifacts = artifact::prepare(&designations, app, version)?;
    session.deliver_with(&request, &artifacts, |api| {
        println!("Images :");
        images::upload(api, app, &prepared, &|path, size| {
            session.check_size(path, size)
        })
    })?;

    // 6 : la commande à transmettre.
    let signing = signed.first().map(|signature| signature.key_id.to_hex());
    let public_key = session
        .app()
        .keys
        .iter()
        .find(|key| {
            signing
                .as_deref()
                .is_some_and(|id| key.key_id.eq_ignore_ascii_case(id))
        })
        .map(|key| key.public_key.clone());
    println!();
    match public_key {
        Some(key) => {
            println!(
                "Installation sur une machine Linux{} : la commande à transmettre, clé publique comprise.",
                if options.publish {
                    ""
                } else {
                    ", une fois la version publiée"
                }
            );
            for line in install_commands(&public_server, app, &key, options.channel) {
                println!("  {line}");
            }
            println!("  sudo drakbase-stack auto on");
        }
        None => println!(
            "La clé qui signe cette version n'est pas déclarée sur le serveur : la commande d'installation n'est pas affichée."
        ),
    }
    Ok(())
}
