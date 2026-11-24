//! Outil `drakbase`, sur le poste du développeur : clés, configuration des apps, signature,
//! empaquetage, publication des mises à jour et fiche publique (docs/PLAN.md, « Outil
//! `drakbase` »).

mod api;
mod artifact;
mod backup;
mod cli_project;
mod connection;
mod credentials;
mod doctor;
mod exec;
mod files;
mod home;
mod init;
mod keepassxc;
mod keygen;
mod listing;
mod login;
mod macos;
mod pack;
mod password;
mod project;
mod publish;
mod register;
mod release;
mod sign;
mod stack;
mod tauri;
mod verify;

#[cfg(test)]
#[path = "../tests/support/fake_server.rs"]
mod fake_server;

use std::io;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};

/// Gabarits de l'aide : intitulés en français, avec l'espace avant les deux-points.
const ROOT_HELP: &str = "{about-with-newline}\nUtilisation : {usage}\n\nCommandes :\n{subcommands}\n\nOptions :\n{options}{after-help}";
const COMMAND_HELP: &str = "{about-with-newline}\nUtilisation : {usage}\n\nArguments :\n{positionals}\n\nOptions :\n{options}{after-help}";
/// Pour une commande sans argument positionnel.
const OPTIONS_HELP: &str =
    "{about-with-newline}\nUtilisation : {usage}\n\nOptions :\n{options}{after-help}";

#[derive(Parser)]
#[command(
    name = "drakbase",
    version,
    about = "Clés, signature, empaquetage et publication des mises à jour de DrakBase - Apps Market",
    help_template = ROOT_HELP,
    subcommand_value_name = "COMMANDE",
    arg_required_else_help = true,
    disable_help_subcommand = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    /// Affiche l'aide
    #[arg(short, long, global = true, action = ArgAction::Help, display_order = 100)]
    help: Option<bool>,

    /// Affiche la version
    #[arg(short = 'V', long, action = ArgAction::Version)]
    version: Option<bool>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Branche les mises à jour dans le projet du dossier courant, app Tauri ou Swift, stack Docker
    /// ou app cli : clés sauvegardées dans KeePassXC, configuration, dépendances, et l'app déclarée
    /// sur le serveur
    #[command(help_template = COMMAND_HELP)]
    Init(init::Args),
    /// Contrôle, sans rien modifier, tout ce dont les mises à jour d'une app ont besoin
    #[command(
        help_template = COMMAND_HELP,
        after_help = "Le code de sortie vaut 1 s'il reste un problème (✗) ; les avertissements (!) ne comptent pas."
    )]
    Doctor(doctor::Args),
    /// Crée la paire de clés d'une app et sa clé de secours, sauvegardées dans KeePassXC, le mot de
    /// passe de la clé principale dans le trousseau
    #[command(help_template = COMMAND_HELP)]
    Keygen(keygen::Args),
    /// Sauvegarde dans KeePassXC les clés d'une app, ou vérifie leur sauvegarde, et range le mot de
    /// passe de la clé principale dans le trousseau
    #[command(help_template = COMMAND_HELP)]
    Backup(backup::BackupArgs),
    /// Réinstalle sur ce Mac les clés d'une app sauvegardées dans KeePassXC
    #[command(help_template = COMMAND_HELP)]
    Restore(backup::RestoreArgs),
    /// Lance une commande, tauri build par exemple, avec la clé principale d'une app et son mot de
    /// passe dans les variables de Tauri
    #[command(help_template = COMMAND_HELP)]
    Exec(exec::Args),
    /// Signe un fichier au format Tauri, avec sa version dans le commentaire de confiance
    #[command(help_template = COMMAND_HELP)]
    Sign(sign::Args),
    /// Vérifie la signature d'un fichier, contrôle par contrôle
    #[command(help_template = COMMAND_HELP)]
    Verify(verify::Args),
    /// Empaquette un bundle macOS signé pour les mises à jour (.app.tar.gz), et son DMG en option
    #[command(help_template = COMMAND_HELP)]
    PackMacos(pack::Args),
    /// Vérifie un jeton d'API auprès du serveur, puis le range dans le trousseau macOS
    #[command(
        help_template = OPTIONS_HELP,
        after_help = "Pour les essais seulement, APPSMARKET_CREDENTIALS=file range l'adresse et le jeton en clair dans credentials.json, dans le dossier de l'outil, à la place du trousseau."
    )]
    Login(login::LoginArgs),
    /// Retire du trousseau l'adresse du serveur et le jeton
    #[command(help_template = OPTIONS_HELP)]
    Logout(login::LogoutArgs),
    /// Envoie les artefacts signés d'une version au serveur, et la publie avec --publish
    #[command(help_template = COMMAND_HELP)]
    Publish(publish::Args),
    /// Empaquette, signe et envoie une version d'un coup : une app macOS à partir de son bundle
    /// signé, ou une stack Docker (--stack), images comprises
    #[command(help_template = COMMAND_HELP)]
    Release(release::Args),
    /// Montre la fiche publique d'une app, sa description et ses captures d'écran, ou la modifie
    #[command(
        help_template = COMMAND_HELP,
        after_help = "Captures : PNG de 320 à 8192 pixels de côté, 8 Mio au plus chacune, 10 au plus, légende de 200 caractères au plus. Description : 4000 caractères au plus. Modifier la fiche demande un jeton avec la portée listing:write."
    )]
    Listing(listing::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init(args) => init::run(args),
        Command::Doctor(args) => doctor::run(args),
        Command::Keygen(args) => keygen::run(args),
        Command::Backup(args) => backup::backup(args),
        Command::Restore(args) => backup::restore(args),
        Command::Exec(args) => exec::run(args),
        Command::Sign(args) => sign::run(args),
        Command::Verify(args) => verify::run(args),
        Command::PackMacos(args) => pack::run(args),
        Command::Login(args) => login::login(args),
        Command::Logout(args) => login::logout(args),
        Command::Publish(args) => publish::run(args),
        Command::Release(args) => release::run(args),
        Command::Listing(args) => listing::run(args),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("erreur : {}", describe(&error));
            ExitCode::FAILURE
        }
    }
}

/// Ajoute à une erreur ce qu'il faut faire ensuite, sur une ligne à part.
fn with_advice(error: anyhow::Error, advice: &str) -> anyhow::Error {
    anyhow::anyhow!("{}\n{advice}", describe(&error))
}

/// Message d'erreur complet, causes comprises. Les erreurs système les plus courantes sont dites
/// en français ; les autres gardent le texte du système.
fn describe(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| match cause.downcast_ref::<io::Error>() {
            Some(io) if io.kind() == io::ErrorKind::NotFound => "introuvable".to_owned(),
            Some(io) if io.kind() == io::ErrorKind::PermissionDenied => "accès refusé".to_owned(),
            Some(io) if io.kind() == io::ErrorKind::AlreadyExists => "existe déjà".to_owned(),
            _ => cause.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" : ")
}
