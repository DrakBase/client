//! Agent `drakbase-stack` : installe et met à jour les stacks Docker de DrakBase sur une machine
//! Linux (docs/PLAN.md, « Stacks Docker » ; docs/PROTOCOLE.md, « Agent `drakbase-stack` »).

mod auto;
mod config;
mod control;
mod docker;
mod envfile;
mod files;
mod http;
mod manage;
mod outcome;
mod paths;
mod prompt;
mod self_update;
mod state;
mod trust;
mod update;

#[cfg(test)]
#[path = "../tests/support/fake_server.rs"]
mod fake_server;

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use am_protocol::signature::PublicKey;
use am_protocol::stack::is_app_slug;
use am_protocol::{Channel, ServerUrl};
use clap::{ArgAction, Args, Parser, Subcommand};

use crate::outcome::{Outcome, UsageError};

/// Gabarits de l'aide : intitulés en français, avec l'espace avant les deux-points.
const ROOT_HELP: &str = "{about-with-newline}\nUtilisation : {usage}\n\nCommandes :\n{subcommands}\n\nOptions :\n{options}{after-help}";
const COMMAND_HELP: &str = "{about-with-newline}\nUtilisation : {usage}\n\nArguments :\n{positionals}\n\nOptions :\n{options}{after-help}";
/// Pour une commande sans argument positionnel.
const OPTIONS_HELP: &str =
    "{about-with-newline}\nUtilisation : {usage}\n\nOptions :\n{options}{after-help}";

const ROOT_AFTER_HELP: &str = "\
Codes de sortie : 0 à jour ou mis à jour, 1 échec, 2 usage, 3 permission à accepter.

Fichiers : /etc/drakbase-stack/config.json, /var/lib/drakbase-stack/<app>/ (state.json, releases/, .env).
Pour les essais, DRAKBASE_STACK_CONFIG_DIR, DRAKBASE_STACK_STATE_DIR et DRAKBASE_STACK_SYSTEMD_DIR
remplacent ces dossiers et celui des unités systemd, DRAKBASE_STACK_DOCKER et DRAKBASE_STACK_SYSTEMCTL
les commandes docker et systemctl.";

#[derive(Parser)]
#[command(
    name = "drakbase-stack",
    version,
    about = "Installe et met à jour les stacks Docker publiées sur DrakBase - Apps Market",
    help_template = ROOT_HELP,
    after_help = ROOT_AFTER_HELP,
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
    /// Installe une stack : vérification de la signature, permissions à accepter, puis démarrage
    #[command(
        help_template = COMMAND_HELP,
        after_help = "La clé vient du message du développeur (drakbase release l'affiche) ou de la page publique de l'app ; prise sur la page, elle ne vaut que la confiance faite au serveur."
    )]
    Install(InstallArgs),
    /// Met à jour une stack, ou toutes les stacks installées, tout de suite
    #[command(
        help_template = COMMAND_HELP,
        after_help = "Une version qui demande une permission nouvelle s'arrête (code 3) jusqu'à update --accept. Une version qui a échoué est écartée : seul update --retry la reprend."
    )]
    Update(UpdateArgs),
    /// Vérifie si une version plus récente existe, la télécharge et la contrôle, sans rien
    /// installer ; l'app la propose ensuite dans son interface
    #[command(help_template = COMMAND_HELP)]
    Check(CheckArgs),
    /// Traite les demandes déposées par l'interface d'une app (lancé par systemd)
    #[command(help_template = COMMAND_HELP)]
    Request(AppArg),
    /// Montre les stacks installées, leurs versions et l'état de leurs services
    #[command(help_template = OPTIONS_HELP)]
    Status,
    /// Montre le journal des services d'une stack
    #[command(help_template = COMMAND_HELP)]
    Logs(LogsArgs),
    /// Relance la version précédente d'une stack et écarte celle qu'il quitte
    #[command(
        help_template = COMMAND_HELP,
        after_help = "La version relancée est contrôlée avec le .env actuel : un réglage qui lui donne une permission nouvelle s'arrête (code 3) jusqu'à rollback --accept."
    )]
    Rollback(RollbackArgs),
    /// Relance une stack avec son .env actuel, après un réglage changé à la main
    #[command(
        help_template = COMMAND_HELP,
        after_help = "Un réglage qui demande une permission nouvelle (un autre port, par exemple) s'arrête (code 3) jusqu'à restart --accept."
    )]
    Restart(RestartArgs),
    /// Arrête et retire une stack ; ses volumes et son .env restent, sauf avec --delete-data
    #[command(help_template = COMMAND_HELP)]
    Uninstall(UninstallArgs),
    /// Active (on) ou coupe (off) la vérification quotidienne des stacks et la réception des
    /// demandes de leur interface ; rien ne s'installe sans qu'on le demande
    #[command(help_template = COMMAND_HELP)]
    Auto(AutoArgs),
    /// Met à jour drakbase-stack lui-même, après vérification de la signature
    #[command(help_template = OPTIONS_HELP)]
    SelfUpdate,
}

#[derive(Args)]
pub struct InstallArgs {
    /// Identifiant de l'app (home-manager, par exemple)
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,

    /// Clé publique de l'app, au format Tauri : l'ancre de confiance
    #[arg(long, value_name = "CLÉ", value_parser = parse_key)]
    pub key: String,

    /// Adresse du serveur [par défaut : celle de config.json, sinon https://updates.drakbase.com]
    #[arg(long, value_name = "ADRESSE", value_parser = ServerUrl::parse)]
    pub server: Option<ServerUrl>,

    /// Canal suivi : stable (par défaut) ou beta, qui reçoit aussi les bêtas
    #[arg(
        long,
        value_name = "CANAL",
        default_value = "stable",
        hide_default_value = true,
        value_parser = parse_channel
    )]
    pub channel: Channel,

    /// N'envoie plus l'identifiant d'installation au serveur (statistiques), pour aucune stack
    #[arg(long)]
    pub no_stats: bool,

    /// Accepte les permissions demandées sans poser la question, pour une installation non
    /// interactive
    #[arg(long)]
    pub accept: bool,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Stack à mettre à jour [par défaut : toutes]
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: Option<String>,

    /// Accepte les permissions nouvelles de la version
    #[arg(long)]
    pub accept: bool,

    /// Reprend une version écartée, après un échec ou un retour arrière
    #[arg(long)]
    pub retry: bool,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Stack à vérifier [par défaut : toutes]
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: Option<String>,
}

#[derive(Args)]
pub struct RestartArgs {
    /// Identifiant de l'app
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,

    /// Accepte les permissions nouvelles que demandent les réglages
    #[arg(long)]
    pub accept: bool,
}

#[derive(Args)]
pub struct RollbackArgs {
    /// Identifiant de l'app
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,

    /// Accepte les permissions nouvelles que les réglages donnent à la version relancée
    #[arg(long)]
    pub accept: bool,
}

#[derive(Args)]
pub struct AppArg {
    /// Identifiant de l'app
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,
}

#[derive(Args)]
pub struct LogsArgs {
    /// Identifiant de l'app
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,

    /// Suit le journal au fil de l'eau
    #[arg(short, long)]
    pub follow: bool,

    /// Nombre de lignes montrées par service (100 par défaut)
    #[arg(
        short = 'n',
        long,
        value_name = "LIGNES",
        default_value_t = 100,
        hide_default_value = true
    )]
    pub tail: u32,
}

#[derive(Args)]
pub struct UninstallArgs {
    /// Identifiant de l'app
    #[arg(value_name = "APP", value_parser = parse_app)]
    pub app: String,

    /// Supprime aussi les volumes de la stack et son .env, après confirmation : les données
    /// sont perdues
    #[arg(long)]
    pub delete_data: bool,
}

#[derive(Args)]
pub struct AutoArgs {
    /// on ou off
    #[arg(value_name = "ÉTAT", value_parser = parse_switch, action = ArgAction::Set)]
    pub state: bool,
}

fn parse_app(value: &str) -> Result<String, String> {
    if is_app_slug(value) {
        Ok(value.to_owned())
    } else {
        Err(
            "identifiant d'app attendu : minuscules, chiffres et tirets, 64 caractères au plus"
                .into(),
        )
    }
}

fn parse_key(value: &str) -> Result<String, String> {
    let value = value.trim();
    PublicKey::from_tauri(value)
        .map(|_| value.to_owned())
        .map_err(|error| format!("clé publique au format Tauri attendue ({error})"))
}

fn parse_channel(value: &str) -> Result<Channel, String> {
    value
        .parse::<Channel>()
        .map_err(|_| "stable ou beta".into())
}

fn parse_switch(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("on ou off".into()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let agent = update::Agent::from_env(io::stdin().is_terminal() && io::stderr().is_terminal());
    let result = match cli.command {
        Command::Install(args) => update::install(&agent, args),
        Command::Update(args) => update::update(&agent, args),
        Command::Check(args) => update::check(&agent, args),
        Command::Request(args) => update::request(&agent, &args.app),
        Command::Status => manage::status(&agent),
        Command::Logs(args) => manage::logs(&agent, args),
        Command::Rollback(args) => manage::rollback(&agent, args),
        Command::Restart(args) => manage::restart(&agent, args),
        Command::Uninstall(args) => manage::uninstall(&agent, args),
        Command::Auto(args) => auto::run(&agent, args.state),
        Command::SelfUpdate => self_update::run(&agent),
    };
    match result {
        Ok(outcome) => ExitCode::from(outcome.code()),
        Err(error) => {
            eprintln!("erreur : {}", outcome::describe(&error));
            if error.downcast_ref::<UsageError>().is_some() {
                ExitCode::from(Outcome::USAGE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
