//! Déclaration d'une app sur le serveur, pour `init`. Le jeton de l'outil ne peut ni créer d'app ni
//! déclarer de clé : `init` ouvre donc l'administration sur une page pré-remplie
//! (`/admin/add-app#…`), où tu crées l'app et actives sa clé d'un clic, avec ta session, le mode
//! sudo et ton code TOTP. L'outil attend ensuite que le serveur montre l'app avec sa clé active.
//! Un jeton ne voit que les apps dont son compte est éditeur, sauf s'il est superadmin : le lien
//! donne alors ce compte à la page, qui l'ajoute comme éditeur dans le même clic.

use std::env;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use am_protocol::signature::KeyId;
use anyhow::{Result, bail};

use crate::api::{self, Api, AppKind, KeyStatus};
use crate::connection::Connection;
use crate::credentials::ServerUrl;
use crate::home::AppSlug;

/// Le programme qui ouvre l'adresse de la page : `open`, sauf si cette variable en désigne un
/// autre, ou vaut `none` pour ne rien ouvrir. Pour les essais.
pub const BROWSER_VAR: &str = "APPSMARKET_BROWSER";
/// Intervalle entre deux questions au serveur, pendant l'attente.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Au-delà, l'outil cesse d'attendre.
const WAIT_MINUTES: u64 = 15;

/// L'app à déclarer, telle que le projet la décrit.
pub struct Registration<'a> {
    pub app: &'a AppSlug,
    pub name: Option<String>,
    pub kind: AppKind,
    pub bundle_id: Option<String>,
    /// La clé publique principale, au format Tauri.
    pub public_key: &'a str,
    pub key_id: KeyId,
}

/// Ce que montre le serveur.
#[derive(Debug, PartialEq)]
enum State {
    /// L'app existe, avec la clé principale active : son nom.
    Ready(String),
    /// L'app n'existe pas, ou n'a aucune clé active : la page le règle.
    Pending,
    /// Le serveur ne répond pas, ou pas comme prévu : peut-être redémarre-t-il.
    Unreachable(String),
}

impl Registration<'_> {
    /// L'adresse de la page, avec l'app et sa clé dans le fragment : le fragment ne part pas vers
    /// le serveur, et ne finit donc dans aucun journal. `editor` est l'adresse du compte du jeton,
    /// quand il n'est pas superadmin : la page le fait éditeur de l'app.
    pub fn page_url(&self, server: &ServerUrl, editor: Option<&str>) -> String {
        let mut fields = vec![("slug", self.app.to_string())];
        if let Some(name) = &self.name {
            fields.push(("name", name.clone()));
        }
        fields.push(("kind", self.kind.as_str().to_owned()));
        if let Some(bundle_id) = &self.bundle_id {
            fields.push(("bundle_id", bundle_id.clone()));
        }
        fields.push(("public_key", self.public_key.to_owned()));
        if let Some(editor) = editor {
            fields.push(("editor", editor.to_owned()));
        }
        let fragment: Vec<String> = fields
            .iter()
            .map(|(name, value)| format!("{name}={}", encode(value)))
            .collect();
        format!("{server}/admin/add-app#{}", fragment.join("&"))
    }

    /// Vérifie que le serveur a l'app avec sa clé principale active ; sinon, ouvre la page qui la
    /// déclare, puis attend. Ce que la page ne réglerait pas (une autre clé active, une app d'un
    /// autre type, un jeton limité à une autre app) est une erreur, sans rien ouvrir.
    pub fn ensure(&self, connection: &Connection) -> Result<()> {
        // Un jeton limité à une autre app ne verrait jamais celle-ci.
        connection.require_app(self.app)?;
        let api = &connection.api;
        match self.state(api)? {
            State::Ready(name) => {
                println!(
                    "✓ app « {name} » sur le serveur, clé {} active",
                    self.key_id
                );
                return Ok(());
            }
            State::Unreachable(problem) => bail!("{} : {problem}", api.server()),
            State::Pending => {}
        }
        let editor = self.editor(connection);
        let url = self.page_url(api.server(), editor.as_deref());
        println!(
            "· l'app {} n'est pas encore prête sur le serveur : j'ouvre l'administration pour la déclarer, avec sa clé {}. Vérifie que la page montre bien cet identifiant, puis valide :",
            self.app, self.key_id
        );
        if let Some(editor) = &editor {
            println!(
                "· le jeton de l'outil est celui de {editor}, qui n'est pas superadmin : il ne voit que les apps dont il est éditeur. La page l'ajoute comme éditeur de {} dans le même clic",
                self.app
            );
        }
        println!("  {url}");
        open(&url);
        println!(
            "En attente de l'administration… (Ctrl-C pour arrêter : « drakbase init {} » reprendra ici)",
            self.app
        );
        let deadline = Instant::now() + Duration::from_secs(WAIT_MINUTES * 60);
        let mut announced = false;
        loop {
            thread::sleep(POLL_INTERVAL);
            match self.state(api)? {
                State::Ready(name) => {
                    println!(
                        "✓ app « {name} » déclarée sur le serveur, clé {} active",
                        self.key_id
                    );
                    return Ok(());
                }
                State::Unreachable(problem) if !announced => {
                    println!("! {} : {problem} (j'attends encore)", api.server());
                    announced = true;
                }
                State::Unreachable(_) | State::Pending => {}
            }
            if Instant::now() >= deadline {
                let editor = editor
                    .as_ref()
                    .map(|editor| format!(" (et {editor} éditeur de l'app)"))
                    .unwrap_or_default();
                bail!(
                    "l'app {} n'est toujours pas prête sur le serveur après {WAIT_MINUTES} minutes : relance « drakbase init {} » une fois la page validée{editor}",
                    self.app,
                    self.app
                );
            }
        }
    }

    /// Le compte du jeton, s'il doit devenir éditeur de l'app pour la voir : tout compte sauf un
    /// superadmin.
    fn editor(&self, connection: &Connection) -> Option<String> {
        let user = &connection.whoami.user;
        (user.role != "superadmin").then(|| user.email.clone())
    }

    fn state(&self, api: &Api) -> Result<State> {
        let app = self.app;
        let info = match api.app(app) {
            Ok(info) => info,
            Err(error) if error.code() == Some("not_found") => return Ok(State::Pending),
            Err(error) if error.code() == Some("forbidden") => bail!(
                "le jeton de l'outil ne donne pas accès à l'app {app} ({error}) : crée un jeton pour toutes tes apps, puis relance « drakbase login »"
            ),
            Err(error) => return Ok(State::Unreachable(error.to_string())),
        };
        if info.kind != self.kind {
            bail!(
                "l'app {app} existe sur le serveur, de type {}, alors que le projet est {} : vérifie l'identifiant de l'app",
                info.kind.as_str(),
                api::described(self.kind)
            );
        }
        let ours = self.key_id.to_hex();
        let key = info
            .keys
            .iter()
            .find(|key| key.key_id.eq_ignore_ascii_case(&ours));
        let active = info.keys.iter().find(|key| key.status == KeyStatus::Active);
        match (key.map(|key| key.status), active) {
            (Some(KeyStatus::Active), _) => Ok(State::Ready(info.name)),
            (_, Some(other)) => bail!(
                "l'app {app} a déjà une autre clé active sur le serveur, {}, alors que le projet embarque la clé {ours} : changer de clé demande une version de transition (onglet Clés de l'administration)",
                other.key_id
            ),
            (Some(status), None) => bail!(
                "la clé {ours} de l'app {app} est « {} » sur le serveur : active-la dans l'onglet Clés de l'administration si c'est bien elle qui signe désormais",
                api::key_status_label(status)
            ),
            (None, None) => Ok(State::Pending),
        }
    }
}

/// Encode une valeur du fragment : tout sauf les lettres, les chiffres et `-._~`. Une clé au
/// format Tauri contient `+`, `/` et `=`, que la page lit avec `URLSearchParams`.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Ouvre l'adresse dans le navigateur, avec `open` ou le programme de `APPSMARKET_BROWSER`.
fn open(url: &str) {
    let program = env::var_os(BROWSER_VAR)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/bin/open".into());
    if program == "none" {
        return;
    }
    let opened = Command::new(&program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        println!("! le navigateur ne s'est pas ouvert : ouvre toi-même l'adresse ci-dessus");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use am_protocol::signature::PublicKey;

    use crate::credentials::{Credentials, Token};
    use crate::fake_server::{FakeKey, FakeServer, State as ServerState};

    /// La connexion de l'outil au faux serveur, avec ce jeton.
    fn connect(server: &FakeServer, token: &str) -> Connection {
        let credentials = Credentials {
            server: ServerUrl::parse(&server.url).unwrap(),
            token: Token::parse(token).unwrap(),
        };
        Connection::connect(credentials)
            .map_err(|error| error.into_error())
            .unwrap()
    }

    fn fixture_public_key() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/keys/test-app.pub"),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    fn swift_app<'a>(app: &'a AppSlug, public_key: &'a str) -> Registration<'a> {
        Registration {
            app,
            name: Some("Démo & co".to_owned()),
            kind: AppKind::Swift,
            bundle_id: Some("fr.example.Demo".to_owned()),
            public_key,
            key_id: PublicKey::from_tauri(public_key).unwrap().key_id(),
        }
    }

    #[test]
    fn the_page_gets_everything_in_its_fragment() {
        let app: AppSlug = "test-app".parse().unwrap();
        let public_key = "dW50cnVzdGVk+/=";
        let registration = Registration {
            app: &app,
            name: Some("Démo & co".to_owned()),
            kind: AppKind::Tauri,
            bundle_id: None,
            public_key,
            key_id: KeyId::from_bytes([0; 8]),
        };
        let server = ServerUrl::parse("https://maj.example.fr").unwrap();
        assert_eq!(
            registration.page_url(&server, None),
            "https://maj.example.fr/admin/add-app#slug=test-app&name=D%C3%A9mo%20%26%20co&kind=tauri&public_key=dW50cnVzdGVk%2B%2F%3D"
        );
        // Le compte du jeton, quand il n'est pas superadmin, en dernier.
        assert!(
            registration
                .page_url(&server, Some("camille+maj@example.fr"))
                .ends_with("&public_key=dW50cnVzdGVk%2B%2F%3D&editor=camille%2Bmaj%40example.fr")
        );
    }

    #[test]
    fn the_page_adds_the_token_account_unless_it_is_a_superadmin() {
        let public_key = fixture_public_key();
        let server = FakeServer::start(ServerState::demo(Some("fr.example.Demo")));
        let app: AppSlug = "nouvelle".parse().unwrap();
        let registration = swift_app(&app, &public_key);

        // Un éditeur : son compte part dans le lien.
        assert_eq!(
            registration.editor(&connect(&server, "amk_essai_complet")),
            Some("camille@example.fr".to_owned())
        );
        // Un superadmin voit toutes les apps : rien à ajouter.
        for token in &mut server.state().tokens {
            token.role = "superadmin".to_owned();
        }
        assert_eq!(
            registration.editor(&connect(&server, "amk_essai_complet")),
            None
        );
        // Un jeton limité à une autre app ne la verrait jamais : rien ne s'ouvre.
        let error = registration
            .ensure(&connect(&server, "amk_essai_autre_app"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ne sert qu'à l'app autre, pas à nouvelle"),
            "{error}"
        );
    }

    #[test]
    fn the_server_says_what_is_left_to_do() {
        let public_key = fixture_public_key();
        let mut state = ServerState::demo(Some("fr.example.Demo"));
        state.apps.get_mut("test-app").unwrap().keys.clear();
        let server = FakeServer::start(state);
        let connection = connect(&server, "amk_essai_complet");
        let api = &connection.api;
        let app: AppSlug = "test-app".parse().unwrap();
        let registration = swift_app(&app, &public_key);

        // Sans clé : la page l'ajoute.
        assert_eq!(registration.state(api).unwrap(), State::Pending);
        // Une app inconnue : la page la crée.
        let other: AppSlug = "inconnue".parse().unwrap();
        assert_eq!(
            swift_app(&other, &public_key).state(api).unwrap(),
            State::Pending
        );

        // La clé active : rien à faire.
        server
            .state()
            .apps
            .get_mut("test-app")
            .unwrap()
            .keys
            .push(FakeKey {
                public_key: public_key.clone(),
                status: "active".to_owned(),
            });
        assert_eq!(
            registration.state(api).unwrap(),
            State::Ready("Test App".to_owned())
        );
        registration.ensure(&connection).unwrap();

        // Une autre clé active, ou une app d'un autre type : la page n'y peut rien.
        let other_key = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/keys/other-app.pub"),
        )
        .unwrap();
        let other_registration = swift_app(&app, other_key.trim());
        let error = other_registration.state(api).unwrap_err().to_string();
        assert!(error.contains("une autre clé active"), "{error}");
        server.state().apps.get_mut("test-app").unwrap().kind = "tauri".to_owned();
        let error = registration.state(api).unwrap_err().to_string();
        assert!(error.contains("de type tauri"), "{error}");
    }
}
