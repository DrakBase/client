//! `drakbase login [--server <url>]` : demande le jeton d'API `amk_…` au terminal, le vérifie
//! auprès du serveur (`GET /api/v1/publish/whoami`), puis range l'adresse et le jeton dans le
//! trousseau macOS. `drakbase logout` les retire.

use std::io;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};

use crate::connection::{self, ConnectError, Connection, PUBLISH_SCOPE, WRITE_SCOPE};
use crate::credentials::{Credentials, Secrets, Token, parse_server};
use crate::home::HomeArgs;
use crate::password::{self, Lines, PasswordSource, Terminal};

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Adresse du serveur, par exemple https://maj.example.fr [défaut : celle de la connexion
    /// actuelle]
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// Lit le jeton sur l'entrée standard plutôt qu'au terminal, pour les scripts
    #[arg(long)]
    token_stdin: bool,

    #[command(flatten)]
    home: HomeArgs,
}

#[derive(clap::Args)]
pub struct LogoutArgs {
    #[command(flatten)]
    home: HomeArgs,
}

pub fn login(args: LoginArgs) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let secrets = Secrets::select(&home)?;
    // Une connexion illisible n'empêche pas d'en enregistrer une nouvelle.
    let current = secrets.login().ok().flatten();
    let server = match (&args.server, &current) {
        (Some(server), _) => parse_server(server)?,
        (None, Some(current)) => {
            println!(
                "Serveur : {} (celui de la connexion actuelle)",
                current.server
            );
            current.server.clone()
        }
        (None, None) if args.token_stdin => {
            bail!(
                "indique l'adresse du serveur avec --server, par exemple --server https://maj.example.fr"
            )
        }
        (None, None) => parse_server(&password::ask_line(
            "Adresse du serveur (https://…) : ",
            "l'adresse du serveur",
            "indique-la avec --server",
        )?)?,
    };
    let typed = if args.token_stdin {
        Lines(io::stdin().lock()).read("")?
    } else {
        Terminal {
            secret: "le jeton",
            otherwise: "utilise --token-stdin",
        }
        .read("Jeton d'API (amk_…) : ")?
    };
    let credentials = Credentials {
        server: server.clone(),
        token: Token::parse(&typed)?,
    };

    let connection = Connection::connect(credentials.clone()).map_err(|error| match error {
        ConnectError::Refused { server, error } => {
            let advice = if error.code() == Some("unauthorized") {
                "Vérifie que le jeton est complet, ou crée-en un autre dans l'administration (Mon compte, Jetons)."
            } else {
                "Vérifie l'adresse du serveur."
            };
            anyhow!(
                "vérification du jeton auprès de {server} : {error}\n{advice} Rien n'a été enregistré."
            )
        }
        other => other.into_error(),
    })?;
    secrets.save_login(&credentials)?;

    let user = &connection.whoami.user;
    let token = &connection.whoami.token;
    println!("Connecté à {server}");
    let name = if user.name.is_empty() {
        user.email.clone()
    } else {
        format!("{} ({})", user.name, user.email)
    };
    println!("  compte : {name}, {}", role_name(&user.role));
    let scopes = if token.scopes.is_empty() {
        "aucune portée".to_owned()
    } else {
        format!("portées {}", enumerate(&token.scopes))
    };
    let apps = match &token.app {
        Some(app) => format!("pour l'app {app}"),
        None => "pour toutes tes apps".to_owned(),
    };
    println!(
        "  jeton  : « {} », {scopes}, {apps}, valable jusqu'au {}",
        token.name,
        connection::day(&token.expires_at)
    );
    println!("Adresse et jeton rangés dans {}.", secrets.login_place());
    if !token.has_scope(WRITE_SCOPE) {
        eprintln!(
            "attention : sans la portée releases:write, ce jeton ne permet pas d'envoyer une version"
        );
    } else if !token.has_scope(PUBLISH_SCOPE) {
        println!(
            "Sans la portée releases:publish, publish et release laisseront les versions en brouillon, à publier depuis l'administration."
        );
    }
    if let Some(warning) = connection::expiry_warning(token) {
        eprintln!("attention : {warning}");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn logout(args: LogoutArgs) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    let secrets = Secrets::select(&home)?;
    match secrets.forget_login()? {
        Some(server) => {
            println!(
                "Déconnecté de {server} : adresse et jeton retirés {}.",
                secrets.login_place_after_de()
            );
            println!(
                "Le jeton reste valable sur le serveur : révoque-le dans l'administration (Mon compte, Jetons) s'il ne doit plus servir."
            );
        }
        None => println!(
            "Aucune connexion enregistrée dans {}.",
            secrets.login_place()
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// « a », « a et b », « a, b et c ».
fn enumerate(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first @ .., last] => format!("{} et {last}", first.join(", ")),
    }
}

fn role_name(role: &str) -> &str {
    match role {
        "editor" => "éditeur",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_read_as_a_list() {
        let scopes = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        assert_eq!(enumerate(&scopes(&["a"])), "a");
        assert_eq!(enumerate(&scopes(&["a", "b"])), "a et b");
        assert_eq!(enumerate(&scopes(&["a", "b", "c"])), "a, b et c");
    }
}
