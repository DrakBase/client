//! `drakbase backup <app>` sauvegarde dans KeePassXC des clés créées avant que l'outil ne le
//! fasse lui-même, ou vérifie une sauvegarde déjà faite, et range au passage le mot de passe de la
//! clé principale dans le trousseau. `drakbase restore <app>` réinstalle les clés depuis
//! KeePassXC, par exemple sur un nouveau Mac. Les sauvegardes vérifiées sont notées dans
//! `backups.json`, que lit `doctor`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::signature::KeyId;
use am_protocol::signing;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::credentials::Secrets;
use crate::files;
use crate::home::{AppSlug, Home, HomeArgs, KeyRole};
use crate::keepassxc::{self, KeePassXc, KeyEntry, VaultArgs};
use crate::keygen;
use crate::password::PasswordSource;
use crate::project;

/// Saisies permises au terminal pour le mot de passe d'une clé existante.
const ATTEMPTS: usize = 3;

#[derive(clap::Args)]
pub struct BackupArgs {
    /// Identifiant de l'app (slug), par exemple translate
    #[arg(value_name = "APP")]
    app: AppSlug,

    /// Lit les mots de passe sur l'entrée standard, un par ligne : celui de la base KeePassXC,
    /// puis ceux des clés à sauvegarder que le trousseau n'a pas, la principale d'abord
    #[arg(long)]
    password_stdin: bool,

    #[command(flatten)]
    vault: VaultArgs,

    #[command(flatten)]
    home: HomeArgs,
}

#[derive(clap::Args)]
pub struct RestoreArgs {
    /// Identifiant de l'app (slug), par exemple translate
    #[arg(value_name = "APP")]
    app: AppSlug,

    /// Réinstalle aussi la clé secrète de secours, pour signer une version de transition si la
    /// clé principale est perdue
    #[arg(long)]
    backup_key: bool,

    /// Lit le mot de passe de la base KeePassXC sur l'entrée standard, plutôt qu'au terminal
    #[arg(long)]
    password_stdin: bool,

    #[command(flatten)]
    vault: VaultArgs,

    #[command(flatten)]
    home: HomeArgs,
}

/// Ce que `backups.json` retient de la dernière sauvegarde vérifiée d'une app.
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    /// La base KeePassXC.
    pub keepassxc: PathBuf,
    /// Quand, en RFC 3339.
    pub saved_at: String,
    /// Identifiant de la clé principale sauvegardée.
    pub main: String,
    /// Identifiant de la clé de secours sauvegardée, s'il y en a une.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

impl Record {
    /// La date seule : `2026-10-28`.
    pub fn day(&self) -> &str {
        self.saved_at
            .split_once('T')
            .map_or(self.saved_at.as_str(), |(day, _)| day)
    }
}

fn load_all(home: &Home) -> Result<BTreeMap<String, Record>> {
    let records = files::read_json(
        &home.backups_file(),
        " : « drakbase backup <app> » le refait",
    )?;
    Ok(records.unwrap_or_default())
}

/// La sauvegarde notée de l'app, s'il y en a une.
pub fn load(home: &Home, app: &AppSlug) -> Result<Option<Record>> {
    Ok(load_all(home)?.remove(&app.to_string()))
}

/// Note la sauvegarde vérifiée des clés `main` et `backup` de l'app, dans cette base.
pub fn remember(
    home: &Home,
    app: &AppSlug,
    vault: &KeePassXc,
    main: KeyId,
    backup: Option<KeyId>,
) -> Result<()> {
    let mut records = load_all(home)?;
    records.insert(
        app.to_string(),
        Record {
            keepassxc: vault.database().to_owned(),
            saved_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
            main: main.to_hex(),
            backup: backup.map(KeyId::to_hex),
        },
    );
    files::write_private_json(&home.backups_file(), &records)
}

pub fn backup(args: BackupArgs) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let app = &args.app;
    let main_files = home.key_files(app, KeyRole::Main);
    let backup_files = home.key_files(app, KeyRole::Backup);
    let main_public = read_key(&main_files.public)?.with_context(|| {
        format!(
            "pas de clé publique principale pour l'app {app} ({}) : « drakbase keygen {app} » crée les clés, « drakbase restore {app} » les réinstalle",
            main_files.public.display()
        )
    })?;
    let main_secret = read_key(&main_files.secret)?.with_context(|| {
        format!(
            "clé secrète principale {} introuvable : « drakbase restore {app} » la réinstalle depuis KeePassXC",
            main_files.secret.display()
        )
    })?;
    let backup_public = read_key(&backup_files.public)?;
    let backup_secret = read_key(&backup_files.secret)?;
    let main_id = project::key_id(&main_public)?;
    let backup_id = backup_public.as_deref().map(project::key_id).transpose()?;

    let mut source = keygen::password_source(args.password_stdin);
    let mut vault = KeePassXc::open(&home, &args.vault, source.as_mut())?;
    let secrets = Secrets::select(&home)?;

    // Ce que KeePassXC garde déjà doit servir, et être la clé de ce Mac.
    let saved_main = vault.read(app, KeyRole::Main)?;
    let saved_backup = vault.read(app, KeyRole::Backup)?;
    for (entry, local) in [(&saved_main, Some(main_id)), (&saved_backup, backup_id)] {
        let Some(entry) = entry else { continue };
        let path = keepassxc::entry_path(app, entry.role);
        entry
            .check()
            .with_context(|| format!("entrée « {path} » de KeePassXC"))?;
        if let Some(local) = local {
            ensure!(
                entry.key_id == local,
                "l'entrée « {path} » de KeePassXC garde la clé {}, et ce Mac la clé {local} : vérifie laquelle est la bonne avant d'aller plus loin",
                entry.key_id
            );
        }
    }

    let stored = secrets.key_password(main_id)?;
    let main_password = opening_password(
        &main_secret,
        main_id,
        &[
            stored.clone(),
            saved_main.as_ref().map(|entry| entry.password.clone()),
        ],
        source.as_mut(),
        &main_files.secret,
    )?;
    let mut new_entries = Vec::new();
    if saved_main.is_none() {
        new_entries.push(KeyEntry::new(
            KeyRole::Main,
            main_password.clone(),
            &main_secret,
            &main_public,
        )?);
    }
    if saved_backup.is_none()
        && let (Some(secret), Some(public), Some(id)) = (&backup_secret, &backup_public, backup_id)
    {
        let password = opening_password(
            secret,
            id,
            &[secrets.key_password(id)?],
            source.as_mut(),
            &backup_files.secret,
        )?;
        new_entries.push(KeyEntry::new(KeyRole::Backup, password, secret, public)?);
    }

    println!(
        "Sauvegarde des clés de l'app {app} dans {}",
        vault.describe()
    );
    if !new_entries.is_empty() {
        vault.save(app, &new_entries.iter().collect::<Vec<_>>())?;
    }
    for role in KeyRole::ALL {
        let path = keepassxc::entry_path(app, role);
        if new_entries.iter().any(|entry| entry.role == role) {
            println!("✓ « {path} » écrite et relue : la clé s'ouvre avec le mot de passe relu");
        } else if vault.has_entry(app, role) {
            println!("✓ « {path} » déjà là, relue : la clé s'ouvre avec son mot de passe");
        }
    }
    if stored.as_deref() != Some(main_password.as_str()) {
        secrets.save_key_password(
            main_id,
            &format!("{} de l'app {app}", KeyRole::Main.label()),
            &main_password,
        )?;
        println!(
            "✓ mot de passe de la clé principale rangé dans {} : les signatures ne le demanderont plus",
            secrets.key_passwords_place()
        );
    }

    let saved_backup_id = saved_backup
        .as_ref()
        .or(new_entries
            .iter()
            .find(|entry| entry.role == KeyRole::Backup))
        .map(|entry| entry.key_id);
    if backup_secret.is_some() && saved_backup_id.is_some() {
        fs::remove_file(&backup_files.secret)
            .with_context(|| format!("suppression de {}", backup_files.secret.display()))?;
        println!(
            "✓ clé secrète de secours retirée de ce Mac ({}) : elle n'est plus que dans KeePassXC",
            backup_files.secret.display()
        );
    }
    remember(&home, app, &vault, main_id, saved_backup_id)?;
    if saved_backup_id.is_none() {
        println!(
            "! pas de clé de secours à sauvegarder, ni sur ce Mac ni dans KeePassXC : les apps Swift qui en embarquent une ne pourront pas s'en servir"
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn restore(args: RestoreArgs) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let app = &args.app;
    let mut source = keygen::password_source(args.password_stdin);
    let vault = KeePassXc::open(&home, &args.vault, source.as_mut())?;
    let main = vault.read(app, KeyRole::Main)?.with_context(|| {
        format!(
            "{} n'a pas de clé principale pour l'app {app} (entrée « {} »)",
            vault.describe(),
            keepassxc::entry_path(app, KeyRole::Main)
        )
    })?;
    let backup = vault.read(app, KeyRole::Backup)?;
    for entry in [Some(&main), backup.as_ref()].into_iter().flatten() {
        entry.check().with_context(|| {
            format!(
                "entrée « {} » de KeePassXC",
                keepassxc::entry_path(app, entry.role)
            )
        })?;
    }
    if args.backup_key && backup.is_none() {
        bail!(
            "{} n'a pas de clé de secours pour l'app {app} (entrée « {} »)",
            vault.describe(),
            keepassxc::entry_path(app, KeyRole::Backup)
        );
    }

    let main_files = home.key_files(app, KeyRole::Main);
    let backup_files = home.key_files(app, KeyRole::Backup);
    let mut wanted: Vec<(&Path, &str, u32)> = vec![
        (main_files.secret.as_path(), main.secret.as_str(), 0o600),
        (main_files.public.as_path(), main.public.as_str(), 0o644),
    ];
    if let Some(entry) = &backup {
        wanted.push((backup_files.public.as_path(), entry.public.as_str(), 0o644));
        if args.backup_key {
            wanted.push((backup_files.secret.as_path(), entry.secret.as_str(), 0o600));
        }
    }
    // Un fichier déjà là doit être celui de KeePassXC : une clé n'est jamais remplacée.
    let mut missing = Vec::new();
    for (path, contents, mode) in wanted {
        match read_key(path)? {
            Some(existing) if existing == contents => {
                println!("· {} : déjà en place", path.display());
            }
            Some(_) => bail!(
                "{} existe déjà et diffère de la clé de KeePassXC : je ne remplace jamais une clé. Si c'est bien celle de KeePassXC qu'il faut, déplace d'abord ce fichier.",
                path.display()
            ),
            None => missing.push((path, contents, mode)),
        }
    }
    home.create_keys_dir()?;
    keygen::write_new(&missing)?;
    for (path, _, _) in &missing {
        println!("✓ {} réinstallé", path.display());
    }

    let secrets = Secrets::select(&home)?;
    let mut stored = vec![&main];
    if args.backup_key {
        stored.extend(&backup);
    }
    for entry in stored {
        secrets.save_key_password(
            entry.key_id,
            &format!("{} de l'app {app}", entry.role.label()),
            &entry.password,
        )?;
        println!(
            "✓ mot de passe de la {} rangé dans {}",
            entry.role.label(),
            secrets.key_passwords_place()
        );
    }
    remember(
        &home,
        app,
        &vault,
        main.key_id,
        backup.as_ref().map(|entry| entry.key_id),
    )?;
    println!(
        "Clés de l'app {app} réinstallées depuis {}.",
        vault.describe()
    );
    if args.backup_key {
        println!(
            "La clé de secours signe avec --key {} (sign, release). Retire-la de ce Mac une fois la version de transition publiée : KeePassXC la garde.",
            backup_files.secret.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Le contenu d'un fichier de clé, sans blanc autour, ou `None` s'il n'existe pas.
fn read_key(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("lecture de {}", path.display())),
    }
}

/// Le mot de passe qui ouvre cette clé secrète : le premier des candidats qui l'ouvre, sinon celui
/// que tu tapes, vérifié lui aussi. Trois essais au terminal.
fn opening_password(
    secret: &str,
    id: KeyId,
    candidates: &[Option<String>],
    source: &mut dyn PasswordSource,
    path: &Path,
) -> Result<String> {
    let opens = |password: &str| signing::secret_key_id(secret, password).ok() == Some(id);
    if let Some(found) = candidates.iter().flatten().find(|password| opens(password)) {
        return Ok(found.clone());
    }
    let attempts = if source.interactive() { ATTEMPTS } else { 1 };
    for _ in 0..attempts {
        let typed = source.read(&format!("Mot de passe de la clé {} : ", path.display()))?;
        if opens(&typed) {
            return Ok(typed);
        }
        if source.interactive() {
            eprintln!("Ce mot de passe n'ouvre pas la clé.");
        }
    }
    bail!(
        "le mot de passe donné n'ouvre pas la clé {} : rien n'a été sauvegardé",
        path.display()
    )
}
