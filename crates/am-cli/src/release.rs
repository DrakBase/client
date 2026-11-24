//! `drakbase release <Nom.app> --app <app> --channel stable|beta [--notes <fichier.md>]
//! [--tauri-config <chemin>] [--publish] [--force] [--prune]` : à partir d'un bundle macOS signé
//! avec ton certificat, enchaîne `pack-macos` (archive et DMG), `sign` et `publish`. La version
//! est celle du bundle, et son arch celle de son exécutable.
//!
//! Avec `--stack [<dossier>] --version <v>`, publie une stack Docker : images construites et
//! envoyées, archive signée par arch (`stack/release.rs`).
//!
//! Le serveur est consulté avant d'empaqueter et de signer : un jeton refusé ou une version trop
//! basse arrêtent tout avant qu'un mot de passe soit demandé.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::{BundleType, Channel, Os, Version};
use anyhow::{Context, Result, bail};

use crate::artifact::{self, Designation, Slot};
use crate::credentials::Secrets;
use crate::home::{AppSlug, Home, HomeArgs};
use crate::macos;
use crate::pack::{self, Bundle};
use crate::publish::{self, Request, Session};
use crate::sign;
use crate::stack;

#[derive(clap::Args)]
pub struct Args {
    /// Bundle de l'app macOS, signé avec ton certificat
    #[arg(
        value_name = "NOM.app",
        required_unless_present = "stack",
        conflicts_with = "stack"
    )]
    bundle: Option<PathBuf>,

    /// App sur le serveur (slug), par exemple translate ; sa clé principale signe l'archive, à
    /// défaut de --key et de TAURI_SIGNING_PRIVATE_KEY
    #[arg(long, value_name = "APP")]
    app: AppSlug,

    /// Publie une stack Docker : son dossier, avec compose.yaml et stack.json [défaut : stack]
    #[arg(
        long,
        value_name = "DOSSIER",
        num_args = 0..=1,
        default_missing_value = stack::DEFAULT_DIR,
        requires = "version"
    )]
    stack: Option<PathBuf>,

    /// Version d'une stack, publiée sous sa forme canonique (1.2 devient 1.2.0) ; celle d'une app
    /// macOS est lue dans son bundle
    #[arg(long, value_name = "VERSION", requires = "stack")]
    version: Option<String>,

    /// Canal de la version : stable ou beta
    #[arg(long, value_name = "CANAL", value_parser = publish::parse_channel)]
    channel: Channel,

    /// Notes de version, en Markdown ; sans cette option, un brouillon repris garde les siennes
    #[arg(long, value_name = "FICHIER.md")]
    notes: Option<PathBuf>,

    #[command(flatten)]
    tauri: publish::TauriConfigArg,

    /// Publie la version une fois les artefacts envoyés ; sans cette option, elle reste en
    /// brouillon
    #[arg(long)]
    publish: bool,

    /// Accepte une version inférieure ou égale à la dernière du canal
    #[arg(long)]
    force: bool,

    /// Retire du brouillon les artefacts envoyés avant qui ne font pas partie de cet envoi
    #[arg(long)]
    prune: bool,

    /// N'envoie pas de DMG de première installation
    #[arg(long, conflicts_with = "stack")]
    no_dmg: bool,

    /// Dossier des archives, de leurs signatures et du DMG, remplacés s'ils existent [défaut :
    /// celui du bundle, ou dist/ à côté du dossier de la stack]
    #[arg(short, long, value_name = "DOSSIER")]
    output: Option<PathBuf>,

    /// Fichier de la clé secrète, à la place de TAURI_SIGNING_PRIVATE_KEY et de la clé de l'app
    #[arg(long, value_name = "FICHIER")]
    key: Option<PathBuf>,

    #[command(flatten)]
    home: HomeArgs,
}

/// Ce que fait `release`, une fois le bundle lu et son certificat contrôlé.
struct Options<'a> {
    app: &'a AppSlug,
    channel: Channel,
    notes: Option<&'a str>,
    tauri_config: Option<&'a Path>,
    publish: bool,
    force: bool,
    prune: bool,
    dmg: bool,
    output: Option<&'a Path>,
    key: Option<&'a Path>,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let notes = args.notes.as_deref().map(publish::read_notes).transpose()?;
    if let Some(dir) = &args.stack {
        if args.tauri.path.is_some() {
            bail!("--tauri-config ne sert qu'aux apps Tauri, pas à une stack");
        }
        let typed = args.version.as_deref().unwrap_or_default().trim();
        let version = Version::parse_release(typed).context("--version")?;
        let secrets = Secrets::select(&home)?;
        stack::release::release(
            &secrets,
            &home,
            &stack::release::Options {
                app: &args.app,
                dir,
                version: &version,
                typed_version: typed,
                channel: args.channel,
                notes: notes.as_deref(),
                publish: args.publish,
                force: args.force,
                prune: args.prune,
                output: args.output.as_deref(),
                key: args.key.as_deref(),
            },
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let Some(bundle) = &args.bundle else {
        bail!("donne le bundle de l'app macOS, ou --stack pour une stack Docker");
    };
    let bundle = pack::read_bundle(bundle)?;
    let authority = pack::require_certificate(&bundle.path)?;
    let secrets = Secrets::select(&home)?;
    let options = Options {
        app: &args.app,
        channel: args.channel,
        notes: notes.as_deref(),
        tauri_config: args.tauri.path.as_deref(),
        publish: args.publish,
        force: args.force,
        prune: args.prune,
        dmg: !args.no_dmg,
        output: args.output.as_deref(),
        key: args.key.as_deref(),
    };
    release(&secrets, &home, &bundle, &authority, &options)?;
    Ok(ExitCode::SUCCESS)
}

fn release(
    secrets: &Secrets,
    home: &Home,
    bundle: &Bundle,
    authority: &str,
    options: &Options<'_>,
) -> Result<()> {
    let version = &bundle.info.version;
    let cpus = macos::bundle_cpus(&bundle.path, &bundle.info)?;
    let arch = cpus.arch().with_context(|| {
        format!(
            "{} : l'exécutable contient {cpus}, ni arm64 ni x86_64",
            bundle.path.display()
        )
    })?;
    let dir = pack::output_dir(bundle, options.output)?;
    let outputs = pack::Outputs::new(bundle, &dir, options.dmg, true)?;

    pack::print_bundle(bundle);
    println!("Processeurs : {cpus}, artefact {arch}");
    println!("Certificat  : {authority}");
    println!();

    let request = Request {
        app: options.app,
        version,
        typed_version: None,
        channel: options.channel,
        notes: options.notes,
        tauri_config: options.tauri_config,
        drakbase_json: None,
        publish: options.publish,
        force: options.force,
        prune: options.prune,
        draft_hint: None,
    };
    let session = Session::open(secrets, &request)?;
    session.check_bundle_id(bundle.info.identifier.as_deref(), &bundle.path)?;
    println!();

    pack::package(bundle, &outputs, true)?;
    let archive_size = fs::metadata(&outputs.archive)?.len();
    println!(
        "✓ Archive : {} ({})",
        outputs.archive.display(),
        am_protocol::human_size(archive_size)
    );
    let dmg_size = outputs
        .dmg
        .as_ref()
        .map(|dmg| fs::metadata(dmg).map(|metadata| (dmg, metadata.len())))
        .transpose()?;
    if let Some((dmg, size)) = dmg_size {
        println!(
            "✓ DMG : {} ({})",
            dmg.display(),
            am_protocol::human_size(size)
        );
    }
    // Avant de demander le mot de passe de la clé : un fichier trop gros n'irait pas plus loin.
    session.check_size(&outputs.archive, archive_size)?;
    if let Some((dmg, size)) = dmg_size {
        session.check_size(dmg, size)?;
    }
    let signed = sign::sign_file(
        &outputs.archive,
        options.app,
        version,
        options.key,
        home,
        secrets,
    )?;
    println!(
        "✓ Signature : {}, clé {}",
        signed.sig_path.display(),
        sign::describe_key(&signed, options.app)
    );
    sign::warn_if_foreign(&signed, options.app, home);

    let mut designations = vec![Designation::at(
        Slot::new(Os::Darwin, arch, BundleType::App)?,
        outputs.archive.clone(),
    )];
    if let Some(dmg) = &outputs.dmg {
        designations.push(Designation::at(
            Slot::new(Os::Darwin, arch, BundleType::Dmg)?,
            dmg.clone(),
        ));
    }
    let artifacts = artifact::prepare(&designations, options.app, version)?;
    session.deliver(&request, &artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::credentials::{Credentials, ServerUrl, Token};
    use crate::fake_server::{FakeServer, State};
    use crate::home::KeyRole;
    use crate::pack::tests::{fake_app, sign_adhoc};
    use crate::project;

    /// Mot de passe public des clés de `fixtures/keys`.
    const FIXTURE_PASSWORD: &str = "appsmarket-fixtures";

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    /// Ce que range `drakbase login`, pour le faux serveur.
    fn save_login(secrets: &Secrets, server: &FakeServer, token: &str) {
        secrets
            .save_login(&Credentials {
                server: ServerUrl::parse(&server.url).unwrap(),
                token: Token::parse(token).unwrap(),
            })
            .unwrap();
    }

    /// Les clés d'essai comme clé principale de l'app, avec son mot de passe dans les secrets,
    /// comme les range `keygen` ; rend le fichier de la clé secrète.
    fn install_key(home: &Home, secrets: &Secrets, app: &AppSlug) -> PathBuf {
        let files = home.key_files(app, KeyRole::Main);
        home.create_keys_dir().unwrap();
        fs::copy(fixtures().join("keys/test-app.secret"), &files.secret).unwrap();
        fs::copy(fixtures().join("keys/test-app.pub"), &files.public).unwrap();
        let key_id = project::key_id(&fs::read_to_string(&files.public).unwrap()).unwrap();
        secrets
            .save_key_password(key_id, "clé d'essai", FIXTURE_PASSWORD)
            .unwrap();
        files.secret
    }

    /// Tout `release`, après le contrôle du certificat : il n'y a pas de certificat de signature
    /// de code dans les tests, et le bundle est signé ad hoc.
    #[test]
    fn release_packs_signs_and_publishes_a_bundle() {
        // Le mot de passe de la clé vient des secrets de l'essai, sauf si l'environnement en
        // impose un autre.
        if std::env::var_os(sign::PASSWORD_VAR).is_some() {
            eprintln!("{} est défini : essai sauté", sign::PASSWORD_VAR);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "1.4");
        sign_adhoc(&app);
        let bundle = pack::read_bundle(&app).unwrap();
        let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
        let home = Home::at(&dir.path().join("outil"));
        let secrets = Secrets::files(&home);
        save_login(&secrets, &server, "amk_essai_complet");
        let slug: AppSlug = "test-app".parse().unwrap();
        let secret = install_key(&home, &secrets, &slug);
        let output = dir.path().join("dist");
        let options = Options {
            app: &slug,
            channel: Channel::Beta,
            notes: Some("- Première bêta"),
            tauri_config: None,
            publish: true,
            force: false,
            prune: false,
            dmg: true,
            output: Some(&output),
            key: Some(&secret),
        };
        release(&secrets, &home, &bundle, "essai ad hoc", &options).unwrap();

        let state = server.state();
        let published = &state.apps["test-app"].releases["1.4.0"];
        assert_eq!(published.status, "published");
        assert_eq!(published.channel, Channel::Beta);
        assert_eq!(published.notes_md, "- Première bêta");
        let mut slots: Vec<String> = published
            .artifacts
            .iter()
            .map(|artifact| artifact.slot())
            .collect();
        slots.sort();
        // /usr/bin/true a des tranches arm64 et x86_64 : l'artefact est universel.
        assert_eq!(slots, ["darwin/universal/app", "darwin/universal/dmg"]);
        let archive = output.join("Demo-1.4.0.app.tar.gz");
        assert!(archive.exists());
        assert!(output.join("Demo-1.4.0.app.tar.gz.sig").exists());
        assert!(output.join("Demo-1.4.0.dmg").exists());
        let uploaded = published
            .artifacts
            .iter()
            .find(|artifact| artifact.bundle_type == BundleType::App)
            .unwrap();
        assert_eq!(uploaded.size, fs::metadata(&archive).unwrap().len());
        assert_eq!(uploaded.filename, "Demo-1.4.0.app.tar.gz");
        drop(state);

        // Une seconde fois : la version existe, publiée ; elle n'est pas plus haute que la
        // dernière bêta, et rien n'est réempaqueté.
        let error = format!(
            "{:#}",
            release(&secrets, &home, &bundle, "essai ad hoc", &options).unwrap_err()
        );
        assert!(error.contains("pas plus haute"), "{error}");
    }

    #[test]
    fn release_stops_before_packing_when_the_bundle_belongs_to_another_app() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "1.0.0");
        sign_adhoc(&app);
        let bundle = pack::read_bundle(&app).unwrap();
        let server = FakeServer::start(State::demo(Some("fr.example.Autre")));
        let home = Home::at(&dir.path().join("outil"));
        let secrets = Secrets::files(&home);
        save_login(&secrets, &server, "amk_essai_complet");
        let slug: AppSlug = "test-app".parse().unwrap();
        let output = dir.path().join("dist");
        let options = Options {
            app: &slug,
            channel: Channel::Stable,
            notes: None,
            tauri_config: None,
            publish: false,
            force: false,
            prune: false,
            dmg: false,
            output: Some(&output),
            key: None,
        };
        let error = format!(
            "{:#}",
            release(&secrets, &home, &bundle, "essai ad hoc", &options).unwrap_err()
        );
        assert!(error.contains("attend fr.example.Autre"), "{error}");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
        assert!(server.state().apps["test-app"].releases.is_empty());
    }
}
