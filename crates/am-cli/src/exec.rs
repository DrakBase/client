//! `drakbase exec <app> -- <commande>` : lance une commande avec la clé principale de l'app et
//! son mot de passe dans `TAURI_SIGNING_PRIVATE_KEY` et `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Le
//! mot de passe vient du trousseau : `tauri build` signe ainsi les archives de mise à jour sans
//! que personne ne le tape ni ne le recopie.

use std::ffi::OsString;
use std::fs;
use std::process::{Command, ExitCode};

use am_protocol::signing;
use anyhow::{Context, Result, bail};

use crate::credentials::Secrets;
use crate::home::{AppSlug, HomeArgs, KeyRole};
use crate::password::{PasswordSource, Terminal};
use crate::project;
use crate::sign::{KEY_VAR, PASSWORD_VAR};

#[derive(clap::Args)]
pub struct Args {
    /// Identifiant de l'app (slug), par exemple translate
    #[arg(value_name = "APP")]
    app: AppSlug,

    #[command(flatten)]
    home: HomeArgs,

    /// La commande et ses arguments, après « -- » : par exemple -- npx tauri build
    #[arg(last = true, required = true, value_name = "COMMANDE")]
    command: Vec<OsString>,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let app = &args.app;
    let main = home.key_files(app, KeyRole::Main);
    let public = fs::read_to_string(&main.public).with_context(|| {
        format!(
            "pas de clé publique principale pour l'app {app} ({}) : « drakbase init {app} » crée les clés",
            main.public.display()
        )
    })?;
    let key_id = project::key_id(&public)?;
    let secret = fs::read_to_string(&main.secret).with_context(|| {
        format!(
            "clé secrète principale {} introuvable : « drakbase restore {app} » la réinstalle depuis KeePassXC",
            main.secret.display()
        )
    })?;
    let secrets = Secrets::select(&home)?;
    let (password, stored) = match secrets.key_password(key_id)? {
        Some(password) => (password, true),
        None => (
            Terminal {
                secret: "le mot de passe",
                otherwise: "« drakbase backup <app> » le range dans le trousseau",
            }
            .read(&format!(
                "Mot de passe de la clé {} : ",
                main.secret.display()
            ))?,
            false,
        ),
    };
    // Vérifié ici : sinon, tauri build ne le dirait qu'à la fin de la construction.
    if signing::secret_key_id(secret.trim(), &password).ok() != Some(key_id) {
        if stored {
            bail!(
                "le mot de passe que garde {} n'ouvre pas la clé {} : « drakbase restore {app} » réinstalle la clé et son mot de passe depuis KeePassXC",
                secrets.key_passwords_place(),
                main.secret.display()
            );
        }
        bail!(
            "ce mot de passe n'ouvre pas la clé {}",
            main.secret.display()
        );
    }

    let (program, arguments) = args
        .command
        .split_first()
        .context("aucune commande à lancer")?;
    eprintln!(
        "drakbase exec : clé principale {key_id} de l'app {app} dans {KEY_VAR}, et son mot de passe dans {PASSWORD_VAR}"
    );
    let status = Command::new(program)
        .args(arguments)
        .env(KEY_VAR, &main.secret)
        .env(PASSWORD_VAR, &password)
        .status()
        .with_context(|| format!("lancement de {}", program.to_string_lossy()))?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        // Arrêtée par un signal, Ctrl-C par exemple.
        None => ExitCode::FAILURE,
    })
}
