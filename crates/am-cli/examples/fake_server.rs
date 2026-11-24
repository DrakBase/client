//! Faux serveur de publication, pour essayer `drakbase login`, `publish`, `release`, `listing`,
//! `init` et `doctor` à la main, sans le vrai serveur :
//!
//! ```text
//! cargo run -p am-cli --example fake_server -- [--listen 127.0.0.1:8765] [--bundle-id <identifiant>]
//! ```
//!
//! Il sert l'app Swift `test-app`, la stack Docker `test-stack` et l'app cli `test-cli`, dont la
//! clé active est la clé d'essai principale de `fixtures/keys` (mot de passe
//! `appsmarket-fixtures`). Tout reste en mémoire, et chaque requête s'affiche avec le statut de sa
//! réponse. Côté outil, `APPSMARKET_CREDENTIALS=file` et un `APPSMARKET_HOME` temporaire évitent
//! d'écrire dans le vrai trousseau.
//!
//! Le registre `/v2/` n'y est pas : le faux serveur reçoit les images d'une stack, mais ne les
//! sert pas.

#[path = "../tests/support/fake_server.rs"]
mod fake_server;

use std::net::SocketAddr;
use std::process::ExitCode;

use fake_server::{FakeServer, State};

fn main() -> ExitCode {
    let mut listen = SocketAddr::from(([127, 0, 0, 1], 8765));
    let mut bundle_id = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next();
        match (arg.as_str(), value) {
            ("--listen", Some(value)) => match value.parse() {
                Ok(address) => listen = address,
                Err(_) => return usage(&format!("adresse « {value} » illisible")),
            },
            ("--bundle-id", Some(value)) => bundle_id = Some(value),
            (other, _) => return usage(&format!("option « {other} » inconnue ou sans valeur")),
        }
    }

    let mut state = State::demo(bundle_id.as_deref());
    state.verbose = true;
    let public_key = state.apps["test-app"].keys[0].public_key.clone();
    state.add_app("test-stack", "Test Stack", "stack", &public_key);
    state.add_app("test-cli", "Test Cli", "cli", &public_key);
    let server = FakeServer::bind(listen, state);
    println!("Faux serveur de publication sur {}", server.url);
    println!(
        "  apps   : test-app (Swift, identifiant de bundle {}), test-stack (stack Docker), test-cli (cli)",
        bundle_id.as_deref().unwrap_or("non vérifié")
    );
    println!("  clé    : fixtures/keys/test-app.secret, mot de passe appsmarket-fixtures");
    println!(
        "  jetons : amk_essai_complet (envoi, publication et fiche), amk_essai_ecriture (envoi seul)"
    );
    println!("Arrêt : Ctrl-C");
    loop {
        std::thread::park();
    }
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("erreur : {problem}");
    eprintln!(
        "utilisation : cargo run -p am-cli --example fake_server -- [--listen 127.0.0.1:8765] [--bundle-id <identifiant>]"
    );
    ExitCode::from(2)
}
