//! `drakbase keygen <app>` : paire de clés de l'app et clé de secours, au format Tauri, avec des
//! mots de passe tirés au hasard que personne ne tape. Les deux clés et leurs mots de passe vont
//! dans KeePassXC, d'où ils sont relus avant toute autre chose ; ce Mac garde la clé principale,
//! son mot de passe dans le trousseau, et les deux clés publiques. La clé secrète de secours ne
//! reste que dans KeePassXC. La clé secrète principale sert aussi de `TAURI_SIGNING_PRIVATE_KEY`
//! pour une app Tauri, avec `drakbase exec`.

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use am_protocol::signature::KeyId;
use am_protocol::signing::generate_keypair;
use anyhow::{Context, Result, anyhow, bail};
use ring::rand::{SecureRandom, SystemRandom};

use crate::backup;
use crate::credentials::Secrets;
use crate::files;
use crate::home::{AppSlug, Home, HomeArgs, KeyRole};
use crate::keepassxc::{self, KeePassXc, KeyEntry, VaultArgs};
use crate::password::{Lines, PasswordSource, Terminal};

/// Octets tirés au hasard pour le mot de passe d'une clé : 160 bits.
const PASSWORD_BYTES: usize = 20;

#[derive(clap::Args)]
pub struct Args {
    /// Identifiant de l'app (slug), par exemple translate
    #[arg(value_name = "APP")]
    app: AppSlug,

    /// Lit le mot de passe de la base KeePassXC sur l'entrée standard, plutôt qu'au terminal
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
    create(&home, app, &args.vault, args.password_stdin)?;
    println!();
    println!(
        "Ensuite : « drakbase init {app} », à la racine du projet de l'app, y inscrit les clés publiques et déclare l'app sur le serveur."
    );
    Ok(ExitCode::SUCCESS)
}

/// Les mots de passe à saisir : celui de la base KeePassXC, puis ceux des clés s'il en faut. Au
/// terminal, ou une ligne chacun sur l'entrée standard.
pub(crate) fn password_source(password_stdin: bool) -> Box<dyn PasswordSource> {
    if password_stdin {
        Box::new(Lines(io::stdin().lock()))
    } else {
        Box::new(Terminal {
            secret: "le mot de passe",
            otherwise: "utilise --password-stdin",
        })
    }
}

/// Crée la paire de clés de l'app et sa clé de secours, les sauvegarde dans KeePassXC et vérifie
/// la sauvegarde, puis écrit sur ce Mac la clé principale et les deux clés publiques, et range le
/// mot de passe de la clé principale dans le trousseau. Refuse si l'un des fichiers existe déjà,
/// ou si KeePassXC a déjà des clés pour cette app : une clé n'est jamais remplacée. `init` s'en
/// sert aussi. Rend l'identifiant de la clé principale.
pub(crate) fn create(
    home: &Home,
    app: &AppSlug,
    vault_args: &VaultArgs,
    password_stdin: bool,
) -> Result<KeyId> {
    let main = home.key_files(app, KeyRole::Main);
    let backup = home.key_files(app, KeyRole::Backup);
    let existing: Vec<String> = [&main.secret, &main.public, &backup.secret, &backup.public]
        .into_iter()
        .filter(|path| path.symlink_metadata().is_ok())
        .map(|path| path.display().to_string())
        .collect();
    if !existing.is_empty() {
        bail!(
            "des clés existent déjà pour l'app {app} ({}) : je ne remplace jamais une clé. Pour en créer de nouvelles, déplace d'abord ces fichiers.",
            existing.join(", ")
        );
    }

    let secrets = Secrets::select(home)?;
    let mut source = password_source(password_stdin);
    let mut vault = KeePassXc::open(home, vault_args, source.as_mut())?;
    let saved: Vec<String> = KeyRole::ALL
        .into_iter()
        .filter(|role| vault.has_entry(app, *role))
        .map(|role| keepassxc::entry_path(app, role))
        .collect();
    if !saved.is_empty() {
        bail!(
            "KeePassXC a déjà des clés pour l'app {app} ({}) : « drakbase restore {app} » les réinstalle sur ce Mac. Je n'en crée pas d'autres.",
            saved.join(", ")
        );
    }

    println!("Création des clés de l'app {app}…");
    let main_key = new_key(KeyRole::Main)?;
    let backup_key = new_key(KeyRole::Backup)?;
    println!("Sauvegarde dans {}…", vault.describe());
    vault.save(app, &[&main_key, &backup_key])?;
    println!(
        "✓ entrées « {} » et « {} » écrites et relues : chaque clé s'ouvre avec le mot de passe relu",
        keepassxc::entry_path(app, KeyRole::Main),
        keepassxc::entry_path(app, KeyRole::Backup)
    );

    let restore = || {
        format!(
            "les clés sont sauvegardées dans KeePassXC : « drakbase restore {app} » les réinstalle"
        )
    };
    let dir = home.create_keys_dir().with_context(restore)?;
    write_new(&[
        (main.secret.as_path(), main_key.secret.as_str(), 0o600),
        (main.public.as_path(), main_key.public.as_str(), 0o644),
        (backup.public.as_path(), backup_key.public.as_str(), 0o644),
    ])
    .with_context(restore)?;
    secrets
        .save_key_password(
            main_key.key_id,
            &format!("{} de l'app {app}", KeyRole::Main.label()),
            &main_key.password,
        )
        .with_context(restore)?;
    backup::remember(home, app, &vault, main_key.key_id, Some(backup_key.key_id))?;

    println!("Clés de l'app {app} créées dans {}", dir.display());
    for (key, secret) in [
        (
            &main_key,
            format!(
                "{}, son mot de passe dans {}",
                main.secret.display(),
                secrets.key_passwords_place()
            ),
        ),
        (
            &backup_key,
            "seulement dans KeePassXC, avec son mot de passe".to_owned(),
        ),
    ] {
        println!();
        println!(
            "{} : identifiant {}",
            capitalized(key.role.label()),
            key.key_id
        );
        println!("  secrète  : {secret}");
        println!("  publique : {}", key.public);
    }
    println!();
    println!("Les clés publiques sont aussi dans les fichiers .key.pub.");
    println!(
        "Ta base KeePassXC garde maintenant les clés de l'app {app} : qu'elle soit sauvegardée hors de ce Mac, comme le reste de tes mots de passe."
    );
    Ok(main_key.key_id)
}

/// Une nouvelle paire de clés, avec un mot de passe tiré au hasard.
fn new_key(role: KeyRole) -> Result<KeyEntry> {
    let password = random_password()?;
    let pair = generate_keypair(&password)?;
    KeyEntry::new(role, password, &pair.secret, &pair.public)
}

/// Un mot de passe de clé tiré au hasard, en hexadécimal : personne ne le tape, il va dans
/// KeePassXC et dans le trousseau.
fn random_password() -> Result<String> {
    let mut bytes = [0u8; PASSWORD_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("le générateur aléatoire du système ne répond pas"))?;
    Ok(hex::encode(bytes))
}

/// Écrit des fichiers de clés, avec leurs droits, sans jamais en remplacer un. Si l'un échoue,
/// ceux déjà écrits sont retirés : soit tous les fichiers existent, soit aucun.
pub(crate) fn write_new(keys: &[(&Path, &str, u32)]) -> Result<()> {
    let mut written: Vec<&Path> = Vec::new();
    for (path, contents, mode) in keys {
        // Sans retour à la ligne final, comme Tauri : `tauri signer sign -f` lit le fichier tel
        // quel.
        if let Err(error) = files::create_new(path, contents.as_bytes(), *mode) {
            for done in &written {
                let _ = fs::remove_file(done);
            }
            return Err(error).with_context(|| format!("écriture de {}", path.display()));
        }
        written.push(path);
    }
    Ok(())
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_random_and_long() {
        let first = random_password().unwrap();
        assert_eq!(first.len(), 2 * PASSWORD_BYTES);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, random_password().unwrap());
    }

    #[test]
    fn written_files_are_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a.key"), dir.path().join("a.key.pub"));
        fs::write(&second, "déjà là").unwrap();
        assert!(
            write_new(&[
                (first.as_path(), "secrète", 0o600),
                (second.as_path(), "publique", 0o644)
            ])
            .is_err()
        );
        assert!(!first.exists());
        assert_eq!(fs::read_to_string(&second).unwrap(), "déjà là");
    }

    #[test]
    fn capitalization_handles_accents() {
        assert_eq!(capitalized("clé principale"), "Clé principale");
        assert_eq!(capitalized("été"), "Été");
        assert_eq!(capitalized(""), "");
    }
}
