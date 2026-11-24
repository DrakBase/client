//! Adresse du serveur et jeton d'API, rangés par `drakbase login`, et mots de passe des clés de
//! signature, rangés par `keygen`, `backup` et `restore` : dans le trousseau macOS, ou, pour les
//! essais seulement, dans des fichiers du dossier de l'outil (`APPSMARKET_CREDENTIALS=file`). Les
//! tests et les essais n'écrivent ainsi jamais dans le vrai trousseau.

use std::collections::BTreeMap;
use std::env::{self, VarError};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use am_protocol::ServerUrl;
use am_protocol::signature::KeyId;
use anyhow::{Context, Result, anyhow, bail, ensure};
use core_foundation::data::CFData;
use security_framework::base::Error as SecurityError;
use security_framework::item::{
    ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, Location, SearchResult,
};
use security_framework::os::macos::keychain::SecKeychain;
use serde::{Deserialize, Serialize};

use crate::files;
use crate::home::Home;

/// Choix du rangement : `keychain` (par défaut) ou `file`.
pub const STORE_VAR: &str = "APPSMARKET_CREDENTIALS";

/// Service des éléments du trousseau. Le compte est l'adresse du serveur, le mot de passe le jeton.
const SERVICE: &str = "appsmarket";
/// Service des mots de passe des clés de signature : un élément par clé, dont le compte est
/// l'identifiant.
const KEY_SERVICE: &str = "appsmarket-key";
/// Clés des attributs rendus par le trousseau : `kSecAttrAccount` et `kSecValueData`.
const ACCOUNT_ATTRIBUTE: &str = "acct";
const DATA_ATTRIBUTE: &str = "v_Data";

// Codes d'erreur de Security.framework (SecBase.h).
const ERR_ITEM_NOT_FOUND: i32 = -25300;
const ERR_USER_CANCELED: i32 = -128;
const ERR_AUTH_FAILED: i32 = -25293;
const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// Lit l'adresse d'un serveur donnée par l'utilisateur, ou rangée par `login`. L'adresse elle-même
/// se lit comme partout (`am_protocol::ServerUrl`) ; l'outil y ajoute deux règles : `https://`
/// est sous-entendu, et `http://` n'est permis que pour un serveur sur ce Mac, puisqu'ailleurs le
/// jeton passerait en clair.
pub fn parse_server(input: &str) -> Result<ServerUrl> {
    let trimmed = input.trim();
    let full = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let url = ServerUrl::parse(&full)
        .map_err(|error| anyhow!("{error}, par exemple https://maj.example.fr"))?;
    ensure!(
        url.is_https() || url.is_loopback(),
        "http:// n'est permis que pour un serveur sur ce Mac (localhost) : ailleurs, le jeton passerait en clair. Utilise https://"
    );
    Ok(url)
}

/// Jeton d'API `amk_…`, créé dans l'administration. Il n'est jamais affiché.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn parse(input: &str) -> Result<Self> {
        let token = input.trim();
        ensure!(!token.is_empty(), "jeton vide");
        ensure!(
            token.starts_with("amk_") && token.len() > "amk_".len(),
            "un jeton d'API commence par amk_ : crée-le dans l'administration, page « Mon compte »"
        );
        ensure!(
            token.len() <= 512 && token.bytes().all(|byte| byte.is_ascii_graphic()),
            "jeton illisible : il ne contient ni espace ni accent"
        );
        Ok(Self(token.to_owned()))
    }

    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(amk_…)")
    }
}

/// Ce que `login` range.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub server: ServerUrl,
    pub token: Token,
}

/// Où l'outil range ses secrets : l'adresse du serveur et le jeton d'une part, les mots de passe
/// des clés de signature de l'autre, chacun sous l'identifiant de sa clé. Le trousseau macOS, ou,
/// pour les essais seulement, des fichiers en clair du dossier de l'outil.
pub struct Secrets(Backend);

enum Backend {
    Keychain(Keychain),
    /// Fichiers en clair, réservés aux essais.
    Files {
        /// `credentials.json` : l'adresse du serveur et le jeton.
        login: PathBuf,
        /// `key-passwords.json` : de l'identifiant de chaque clé à son mot de passe.
        key_passwords: PathBuf,
    },
}

/// Contenu de `credentials.json`.
#[derive(Serialize, Deserialize)]
struct StoredLogin {
    server: String,
    token: String,
}

impl Secrets {
    /// Le trousseau, sauf si `APPSMARKET_CREDENTIALS=file` demande les fichiers du dossier de
    /// l'outil.
    pub fn select(home: &Home) -> Result<Self> {
        match env::var(STORE_VAR) {
            Err(VarError::NotPresent) => Ok(Self(Backend::Keychain(Keychain::user()))),
            Ok(value) => match value.trim() {
                "" | "keychain" => Ok(Self(Backend::Keychain(Keychain::user()))),
                "file" => Ok(Self::files(home)),
                other => bail!("{STORE_VAR} vaut « {other} » : keychain ou file attendu"),
            },
            Err(VarError::NotUnicode(_)) => bail!("{STORE_VAR} n'est pas en UTF-8"),
        }
    }

    /// Les fichiers du dossier de l'outil, en clair : pour les essais.
    pub fn files(home: &Home) -> Self {
        Self(Backend::Files {
            login: home.credentials_file(),
            key_passwords: home.key_passwords_file(),
        })
    }

    /// Où est rangée la connexion, pour les messages : « le trousseau macOS », « le fichier … ».
    pub fn login_place(&self) -> String {
        match &self.0 {
            Backend::Keychain(_) => "le trousseau macOS".to_owned(),
            Backend::Files { login, .. } => format!("le fichier {}", login.display()),
        }
    }

    /// La même chose, après « de » : « du trousseau macOS », « du fichier … ».
    pub fn login_place_after_de(&self) -> String {
        match &self.0 {
            Backend::Keychain(_) => "du trousseau macOS".to_owned(),
            Backend::Files { login, .. } => format!("du fichier {}", login.display()),
        }
    }

    /// Où sont rangés les mots de passe des clés, pour les messages.
    pub fn key_passwords_place(&self) -> String {
        match &self.0 {
            Backend::Keychain(_) => "le trousseau macOS".to_owned(),
            Backend::Files { key_passwords, .. } => {
                format!("le fichier {}", key_passwords.display())
            }
        }
    }

    /// La même chose, après « de » : « du trousseau macOS », « du fichier … ».
    pub fn key_passwords_place_after_de(&self) -> String {
        match &self.0 {
            Backend::Keychain(_) => "du trousseau macOS".to_owned(),
            Backend::Files { key_passwords, .. } => {
                format!("du fichier {}", key_passwords.display())
            }
        }
    }

    /// L'adresse du serveur et le jeton rangés par `login`, s'il y en a.
    pub fn login(&self) -> Result<Option<Credentials>> {
        let stored = match &self.0 {
            Backend::Keychain(keychain) => keychain.find(SERVICE, None)?,
            Backend::Files { login, .. } => {
                files::read_json::<StoredLogin>(login, " : refais « drakbase login »")?
                    .map(|stored| (stored.server, stored.token))
            }
        };
        stored
            .map(|(server, token)| {
                let origin = self.login_place_after_de();
                Ok(Credentials {
                    server: parse_server(&server)
                        .with_context(|| format!("adresse du serveur {origin}"))?,
                    token: Token::parse(&token).with_context(|| format!("jeton {origin}"))?,
                })
            })
            .transpose()
    }

    /// Range l'adresse et le jeton, à la place de ce qui l'était.
    pub fn save_login(&self, credentials: &Credentials) -> Result<()> {
        let server = credentials.server.as_str();
        let token = credentials.token.secret();
        match &self.0 {
            Backend::Keychain(keychain) => keychain.put(
                SERVICE,
                None,
                server,
                &format!("DrakBase - Apps Market : {server}"),
                token,
            ),
            Backend::Files { login, .. } => files::write_private_json(
                login,
                &StoredLogin {
                    server: server.to_owned(),
                    token: token.to_owned(),
                },
            ),
        }
    }

    /// Retire la connexion rangée, même illisible, et rend l'adresse du serveur oublié.
    pub fn forget_login(&self) -> Result<Option<String>> {
        let unreadable = || "un serveur illisible".to_owned();
        match &self.0 {
            Backend::Keychain(keychain) => {
                let server = keychain.find(SERVICE, None).ok().flatten();
                let removed = keychain.remove(SERVICE, None)?;
                Ok(server
                    .map(|(server, _)| server)
                    .or_else(|| removed.then(unreadable)))
            }
            Backend::Files { login, .. } => {
                let server = files::read_json::<StoredLogin>(login, "")
                    .ok()
                    .flatten()
                    .map(|stored| stored.server)
                    .or_else(|| login.exists().then(unreadable));
                match fs::remove_file(login) {
                    Ok(()) => Ok(server),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => {
                        Err(error).with_context(|| format!("suppression de {}", login.display()))
                    }
                }
            }
        }
    }

    /// Le mot de passe de cette clé, s'il est rangé. Le trousseau peut demander l'autorisation de
    /// le lire, par exemple après une mise à jour de l'outil.
    pub fn key_password(&self, key: KeyId) -> Result<Option<String>> {
        match &self.0 {
            Backend::Keychain(keychain) => Ok(keychain
                .find(KEY_SERVICE, Some(&key.to_hex()))?
                .map(|(_, password)| password)),
            Backend::Files { key_passwords, .. } => {
                Ok(read_passwords(key_passwords)?.remove(&key.to_hex()))
            }
        }
    }

    /// Vrai si un mot de passe est rangé pour cette clé. Le trousseau le dit sans rien demander :
    /// seul le contenu d'un élément est protégé.
    pub fn has_key_password(&self, key: KeyId) -> Result<bool> {
        match &self.0 {
            Backend::Keychain(keychain) => keychain.has(KEY_SERVICE, &key.to_hex()),
            Backend::Files { key_passwords, .. } => {
                Ok(read_passwords(key_passwords)?.contains_key(&key.to_hex()))
            }
        }
    }

    /// Range le mot de passe de cette clé, à la place de l'ancien. `label` nomme l'élément dans
    /// Trousseau d'accès, par exemple « clé principale de l'app translate ».
    pub fn save_key_password(&self, key: KeyId, label: &str, password: &str) -> Result<()> {
        let id = key.to_hex();
        match &self.0 {
            Backend::Keychain(keychain) => keychain.put(
                KEY_SERVICE,
                Some(&id),
                &id,
                &format!("DrakBase - Apps Market : {label} ({key})"),
                password,
            ),
            Backend::Files { key_passwords, .. } => {
                let mut passwords = read_passwords(key_passwords)?;
                passwords.insert(id, password.to_owned());
                files::write_private_json(key_passwords, &passwords)
            }
        }
    }
}

/// `key-passwords.json` : un objet JSON, de l'identifiant de chaque clé à son mot de passe.
fn read_passwords(path: &Path) -> Result<BTreeMap<String, String>> {
    Ok(files::read_json(path, "")?.unwrap_or_default())
}

/// Un trousseau macOS : celui de l'utilisateur, ou un trousseau jetable dans les tests.
struct Keychain(Option<SecKeychain>);

impl Keychain {
    /// Le trousseau par défaut de l'utilisateur, d'ordinaire « session ».
    fn user() -> Self {
        Self(None)
    }

    fn open(&self) -> Result<SecKeychain> {
        match &self.0 {
            Some(keychain) => Ok(keychain.clone()),
            None => {
                SecKeychain::default().map_err(|error| failure(error, "ouverture du trousseau"))
            }
        }
    }

    /// Les éléments de ce service, et de ce seul compte s'il est donné, dans ce seul trousseau.
    fn query(keychain: &SecKeychain, service: &str, account: Option<&str>) -> ItemSearchOptions {
        let mut query = ItemSearchOptions::new();
        query
            .class(ItemClass::generic_password())
            .service(service)
            .keychains(std::slice::from_ref(keychain));
        if let Some(account) = account {
            query.account(account);
        }
        query
    }

    /// Le premier élément trouvé : son compte et son contenu.
    fn find(&self, service: &str, account: Option<&str>) -> Result<Option<(String, String)>> {
        let keychain = self.open()?;
        // Sans limite, le trousseau rend le premier élément trouvé, attributs et contenu ensemble.
        let found = match Self::query(&keychain, service, account)
            .load_attributes(true)
            .load_data(true)
            .search()
        {
            Ok(found) => found,
            Err(error) if error.code() == ERR_ITEM_NOT_FOUND => return Ok(None),
            Err(error) => return Err(failure(error, "lecture du trousseau")),
        };
        let item = found
            .first()
            .and_then(SearchResult::simplify_dict)
            .with_context(|| format!("élément « {service} » du trousseau illisible"))?;
        let field = |name: &str| {
            item.get(name)
                .cloned()
                .with_context(|| format!("élément « {service} » du trousseau incomplet"))
        };
        Ok(Some((field(ACCOUNT_ATTRIBUTE)?, field(DATA_ATTRIBUTE)?)))
    }

    /// Vrai si l'élément existe, sans lire son contenu, que protège le trousseau.
    fn has(&self, service: &str, account: &str) -> Result<bool> {
        let keychain = self.open()?;
        match Self::query(&keychain, service, Some(account))
            .load_attributes(true)
            .search()
        {
            Ok(found) => Ok(!found.is_empty()),
            Err(error) if error.code() == ERR_ITEM_NOT_FOUND => Ok(false),
            Err(error) => Err(failure(error, "lecture du trousseau")),
        }
    }

    /// Range un élément, à la place de ceux de `replaced` : un seul compte, ou tout le service
    /// (`None`), qui n'a alors qu'un élément à la fois.
    fn put(
        &self,
        service: &str,
        replaced: Option<&str>,
        account: &str,
        label: &str,
        secret: &str,
    ) -> Result<()> {
        let keychain = self.open()?;
        Self::delete_all(|| Self::query(&keychain, service, replaced))?;
        let mut item = ItemAddOptions::new(ItemAddValue::Data {
            class: ItemClass::generic_password(),
            data: CFData::from_buffer(secret.as_bytes()),
        });
        item.set_service(service)
            .set_account_name(account)
            .set_label(label)
            .set_location(Location::FileKeychain(keychain));
        item.add()
            .map_err(|error| failure(error, "écriture dans le trousseau"))
    }

    /// Retire les éléments de ce service, ou de ce seul compte ; rend `true` si au moins un l'a
    /// été.
    fn remove(&self, service: &str, account: Option<&str>) -> Result<bool> {
        let keychain = self.open()?;
        Self::delete_all(|| Self::query(&keychain, service, account))
    }

    /// Retire les éléments de la requête ; rend `true` si au moins un l'a été.
    fn delete_all(query: impl Fn() -> ItemSearchOptions) -> Result<bool> {
        // Un appel ne retire parfois qu'un élément : on recommence jusqu'à n'en plus trouver.
        let mut removed = false;
        for _ in 0..100 {
            match query().delete() {
                Ok(()) => removed = true,
                Err(error) if error.code() == ERR_ITEM_NOT_FOUND => return Ok(removed),
                Err(error) => return Err(failure(error, "suppression dans le trousseau")),
            }
        }
        bail!("trop d'éléments semblables dans le trousseau")
    }
}

fn failure(error: SecurityError, action: &str) -> anyhow::Error {
    match error.code() {
        ERR_USER_CANCELED | ERR_AUTH_FAILED => anyhow!("{action} : accès refusé"),
        ERR_INTERACTION_NOT_ALLOWED => anyhow!(
            "{action} : le trousseau est verrouillé, et aucune fenêtre ne peut demander son mot de passe (session SSH ?). Déverrouille-le avec « security unlock-keychain »."
        ),
        code => anyhow!("{action} : {error} (code {code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use security_framework::os::macos::keychain::CreateOptions;

    fn credentials(server: &str, token: &str) -> Credentials {
        Credentials {
            server: parse_server(server).unwrap(),
            token: Token::parse(token).unwrap(),
        }
    }

    /// Un mot de passe par clé, que le dernier rangé remplace : le même contrat pour le trousseau
    /// et pour les fichiers.
    fn check_key_passwords(secrets: &Secrets) {
        let (first, second) = (KeyId::from_bytes([1; 8]), KeyId::from_bytes([2; 8]));
        assert!(!secrets.has_key_password(first).unwrap());
        assert_eq!(secrets.key_password(first).unwrap(), None);
        secrets
            .save_key_password(first, "clé principale de l'app a", "premier")
            .unwrap();
        secrets
            .save_key_password(second, "clé principale de l'app b", "second")
            .unwrap();
        secrets
            .save_key_password(first, "clé principale de l'app a", "remplacé")
            .unwrap();
        assert!(secrets.has_key_password(first).unwrap());
        assert_eq!(
            secrets.key_password(first).unwrap().as_deref(),
            Some("remplacé")
        );
        assert_eq!(
            secrets.key_password(second).unwrap().as_deref(),
            Some("second")
        );
    }

    #[test]
    fn server_addresses_are_reduced_to_their_origin() {
        for (input, origin) in [
            ("https://maj.example.fr", "https://maj.example.fr"),
            (" https://maj.example.fr/ ", "https://maj.example.fr"),
            ("maj.example.fr", "https://maj.example.fr"),
            ("HTTPS://Maj.Example.FR:8443", "https://maj.example.fr:8443"),
            ("http://localhost:8080", "http://localhost:8080"),
            ("http://127.0.0.1:8765/", "http://127.0.0.1:8765"),
            ("http://[::1]:9000", "http://[::1]:9000"),
        ] {
            assert_eq!(parse_server(input).unwrap().as_str(), origin, "{input}");
        }
        for (input, message) in [
            ("http://maj.example.fr", "en clair"),
            ("http://192.168.1.10:8080", "en clair"),
            ("https://maj.example.fr/admin", "sans chemin"),
            ("https://maj.example.fr/?a=1", "sans chemin"),
            ("https://moi:secret@maj.example.fr", "pas d'identifiant"),
            ("ftp://maj.example.fr", "http ou https attendu"),
            ("https://", "illisible"),
            ("deux mots", "par exemple https://maj.example.fr"),
        ] {
            let error = parse_server(input).unwrap_err().to_string();
            assert!(error.contains(message), "{input} : {error}");
        }
        assert!(parse_server("http://localhost:1").unwrap().is_loopback());
        assert!(
            !parse_server("https://maj.example.fr")
                .unwrap()
                .is_loopback()
        );
    }

    #[test]
    fn tokens_are_checked_and_never_shown() {
        let token = Token::parse("  amk_0123abcd\n").unwrap();
        assert_eq!(token.secret(), "amk_0123abcd");
        assert_eq!(format!("{token:?}"), "Token(amk_…)");
        for invalid in ["", "amk_", "abc_0123", "amk_avec espace", "amk_é"] {
            assert!(Token::parse(invalid).is_err(), "{invalid}");
        }
    }

    /// Les messages nomment le lieu tel quel (« dans le trousseau »), ou après « de » (« absent
    /// du trousseau »).
    #[test]
    fn places_are_named_alone_and_after_de() {
        let keychain = Secrets(Backend::Keychain(Keychain::user()));
        assert_eq!(keychain.login_place(), "le trousseau macOS");
        assert_eq!(keychain.login_place_after_de(), "du trousseau macOS");
        assert_eq!(keychain.key_passwords_place(), "le trousseau macOS");
        assert_eq!(
            keychain.key_passwords_place_after_de(),
            "du trousseau macOS"
        );
        let files = Secrets::files(&Home::at(Path::new("/essai/outil")));
        assert_eq!(
            files.login_place_after_de(),
            "du fichier /essai/outil/credentials.json"
        );
        assert_eq!(
            files.key_passwords_place(),
            "le fichier /essai/outil/key-passwords.json"
        );
        assert_eq!(
            files.key_passwords_place_after_de(),
            "du fichier /essai/outil/key-passwords.json"
        );
    }

    #[test]
    fn the_file_store_is_private_and_replaced_whole() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("outil");
        let store = Secrets::files(&Home::at(&dir));
        assert!(store.login().unwrap().is_none());
        assert_eq!(store.forget_login().unwrap(), None);

        store
            .save_login(&credentials("http://127.0.0.1:8765", "amk_premier"))
            .unwrap();
        store
            .save_login(&credentials("https://maj.example.fr", "amk_second"))
            .unwrap();
        let path = dir.join("credentials.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let loaded = store.login().unwrap().unwrap();
        assert_eq!(loaded.server.as_str(), "https://maj.example.fr");
        assert_eq!(loaded.token.secret(), "amk_second");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        assert_eq!(
            store.forget_login().unwrap().as_deref(),
            Some("https://maj.example.fr")
        );
        assert!(!path.exists());

        fs::write(&path, "{").unwrap();
        let error = store.login().unwrap_err().to_string();
        assert!(error.contains("drakbase login"), "{error}");
        // Un fichier illisible se retire quand même.
        assert_eq!(
            store.forget_login().unwrap().as_deref(),
            Some("un serveur illisible")
        );
        assert!(!path.exists());
    }

    #[test]
    fn key_passwords_are_kept_per_key_in_a_private_file() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("outil/key-passwords.json");
        let passwords = Secrets::files(&Home::at(&parent.path().join("outil")));
        check_key_passwords(&passwords);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// Le vrai code du trousseau, sur un trousseau jetable créé dans un dossier temporaire. Un
    /// trousseau privé n'entre pas dans la liste de recherche de l'utilisateur (seuls ceux de
    /// session et du système y sont ajoutés à la création) : le contrôle final le vérifie.
    #[test]
    fn the_keychain_store_keeps_one_item() {
        let search_list = || {
            let output = Command::new("security")
                .args(["list-keychains", "-d", "user"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let before = search_list();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("essai-appsmarket.keychain");
        let keychain = CreateOptions::new()
            .password("mot-de-passe-du-trousseau-jetable")
            .create(&path)
            .unwrap();
        let store = Secrets(Backend::Keychain(Keychain(Some(keychain))));

        assert!(store.login().unwrap().is_none());
        assert_eq!(store.forget_login().unwrap(), None);
        store
            .save_login(&credentials("http://127.0.0.1:8765", "amk_premier"))
            .unwrap();
        store
            .save_login(&credentials("https://maj.example.fr", "amk_second"))
            .unwrap();
        let loaded = store.login().unwrap().unwrap();
        assert_eq!(loaded.server.as_str(), "https://maj.example.fr");
        assert_eq!(loaded.token.secret(), "amk_second");

        // Les mots de passe des clés : un élément par clé, à côté du jeton, sans le toucher.
        check_key_passwords(&store);
        assert_eq!(store.login().unwrap().unwrap().token.secret(), "amk_second");

        assert_eq!(
            store.forget_login().unwrap().as_deref(),
            Some("https://maj.example.fr")
        );
        assert!(store.login().unwrap().is_none());
        let second = KeyId::from_bytes([2; 8]);
        assert_eq!(
            store.key_password(second).unwrap().as_deref(),
            Some("second")
        );

        assert_eq!(search_list(), before, "liste de recherche modifiée");
        assert!(!before.contains("essai-appsmarket"), "{before}");
    }
}
