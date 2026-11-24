//! Sauvegarde des clés dans KeePassXC, par `keepassxc-cli`, l'outil en ligne de commande fourni
//! avec l'app. Chaque clé y a son entrée, dans le groupe « Drakbase - Apps Market » de ta base :
//! l'identifiant de la clé en nom d'utilisateur, son mot de passe, et dans les notes la clé secrète
//! et la clé publique au format Tauri. Une entrée écrite est relue, et sa clé ouverte avec le mot
//! de passe relu : une sauvegarde ne compte que si elle sert.
//!
//! Le mot de passe de la base est demandé une fois par commande, puis passé à keepassxc-cli sur
//! son entrée standard ; il n'est jamais écrit. La base choisie est retenue dans `config.json`,
//! dans le dossier de l'outil.

use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use am_protocol::signature::{KeyId, PublicKey};
use am_protocol::signing;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::home::{AppSlug, Home, KeyRole};
use crate::password::{self, PasswordSource};
use crate::sign;

/// Groupe des entrées, à la racine de la base.
pub const GROUP: &str = "Drakbase - Apps Market";
/// Chemin de keepassxc-cli, à la place de la recherche : pour une installation ailleurs que dans
/// Applications.
pub const CLI_VAR: &str = "APPSMARKET_KEEPASSXC_CLI";
/// keepassxc-cli est dans l'app.
const CLI_IN_APP: &str = "KeePassXC.app/Contents/MacOS/keepassxc-cli";

/// Options des commandes qui sauvegardent ou relisent les clés dans KeePassXC.
#[derive(clap::Args, Clone, Default)]
pub struct VaultArgs {
    /// Base KeePassXC des sauvegardes (.kdbx), retenue pour les fois suivantes [défaut : celle
    /// retenue, sinon demandée]
    #[arg(long = "keepassxc", value_name = "FICHIER")]
    database: Option<PathBuf>,

    /// Fichier clé de la base KeePassXC, si elle en demande un ; retenu aussi
    #[arg(long = "keepassxc-key-file", value_name = "FICHIER")]
    key_file: Option<PathBuf>,

    /// YubiKey de la base KeePassXC, si elle en demande une : son emplacement, suivi de « : » et
    /// de son numéro de série s'il le faut ; retenue aussi
    #[arg(long = "keepassxc-yubikey", value_name = "EMPLACEMENT")]
    yubikey: Option<String>,
}

impl VaultArgs {
    #[cfg(test)]
    pub fn database(path: &Path) -> Self {
        Self {
            database: Some(path.to_owned()),
            ..Self::default()
        }
    }

    /// La base des options, sinon celle retenue, sinon celle que tu indiques, avec la dernière
    /// qu'a ouverte KeePassXC comme proposition.
    fn settings(&self, home: &Home) -> Result<Settings> {
        let remembered = load_config(home)?.keepassxc;
        let mut settings = match (&self.database, remembered) {
            (Some(database), Some(remembered)) if remembered.database == *database => remembered,
            (Some(database), _) => Settings::of(database.clone()),
            (None, Some(remembered)) => remembered,
            (None, None) => Settings::of(ask_database()?),
        };
        if self.key_file.is_some() {
            settings.key_file.clone_from(&self.key_file);
        }
        if self.yubikey.is_some() {
            settings.yubikey.clone_from(&self.yubikey);
        }
        Ok(settings)
    }
}

/// La base KeePassXC, et ce qui l'ouvre en plus de son mot de passe.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Settings {
    database: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    yubikey: Option<String>,
}

impl Settings {
    fn of(database: PathBuf) -> Self {
        Self {
            database,
            key_file: None,
            yubikey: None,
        }
    }
}

/// `config.json` du dossier de l'outil.
#[derive(Default, Serialize, Deserialize)]
struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keepassxc: Option<Settings>,
}

fn load_config(home: &Home) -> Result<Config> {
    let config = files::read_json(
        &home.config_file(),
        " : corrige-le, ou retire-le pour que l'outil redemande la base KeePassXC",
    )?;
    Ok(config.unwrap_or_default())
}

/// Retient la base pour les fois suivantes, si elle a changé.
fn remember(home: &Home, settings: &Settings) -> Result<()> {
    let mut config = load_config(home)?;
    if config.keepassxc.as_ref() == Some(settings) {
        return Ok(());
    }
    config.keepassxc = Some(settings.clone());
    files::write_private_json(&home.config_file(), &config)?;
    println!(
        "Base KeePassXC retenue pour les fois suivantes : {}",
        settings.database.display()
    );
    Ok(())
}

/// Demande la base au terminal ; une réponse vide prend la dernière qu'a ouverte KeePassXC.
fn ask_database() -> Result<PathBuf> {
    let guess = last_database();
    let prompt = match &guess {
        Some(path) => format!(
            "Base KeePassXC où sauvegarder les clés [{}] : ",
            path.display()
        ),
        None => "Base KeePassXC où sauvegarder les clés (fichier .kdbx) : ".to_owned(),
    };
    let answer = password::ask_line(
        &prompt,
        "la base KeePassXC",
        "indique-la avec --keepassxc <fichier.kdbx>",
    )?;
    match (typed_path(&answer), guess) {
        (Some(path), _) | (None, Some(path)) => Ok(path),
        (None, None) => bail!("aucune base KeePassXC indiquée"),
    }
}

/// Un chemin tapé, ou glissé depuis le Finder : le Terminal protège alors les espaces par des
/// barres obliques inverses, et ajoute une espace finale. `~/` désigne le dossier personnel.
fn typed_path(answer: &str) -> Option<PathBuf> {
    let text = answer.trim().trim_matches(['"', '\'']).replace("\\ ", " ");
    if text.is_empty() {
        return None;
    }
    match (text.strip_prefix("~/"), env::home_dir()) {
        (Some(rest), Some(home)) => Some(home.join(rest)),
        _ => Some(PathBuf::from(text)),
    }
}

/// La dernière base qu'a ouverte KeePassXC, d'après ses réglages, si elle existe encore.
fn last_database() -> Option<PathBuf> {
    let home = env::home_dir()?;
    [
        "Library/Caches/KeePassXC/keepassxc.ini",
        "Library/Application Support/KeePassXC/keepassxc.ini",
    ]
    .iter()
    .filter_map(|file| fs::read_to_string(home.join(file)).ok())
    .find_map(|text| parse_last_database(&text))
    .filter(|path| path.is_file())
}

/// `LastDatabases=` des réglages de KeePassXC : une liste séparée par des virgules, la plus
/// récente d'abord.
fn parse_last_database(ini: &str) -> Option<PathBuf> {
    let value = ini
        .lines()
        .find_map(|line| line.trim().strip_prefix("LastDatabases="))?;
    let first = value.split(", ").next()?.trim().trim_matches('"');
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// keepassxc-cli : celui de `APPSMARKET_KEEPASSXC_CLI`, sinon celui de l'app, sinon celui du PATH.
fn find_cli() -> Result<PathBuf> {
    if let Some(path) = env::var_os(CLI_VAR).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let mut candidates = vec![Path::new("/Applications").join(CLI_IN_APP)];
    if let Some(home) = env::home_dir() {
        candidates.push(home.join("Applications").join(CLI_IN_APP));
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|dir| dir.join("keepassxc-cli")));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| {
            format!(
                "keepassxc-cli introuvable : installe KeePassXC (https://keepassxc.org) dans Applications, ou indique son outil avec {CLI_VAR}"
            )
        })
}

/// Titre de l'entrée d'une clé : « translate - clé principale ».
fn title(app: &AppSlug, role: KeyRole) -> String {
    format!("{app} - {}", role.label())
}

/// Chemin de l'entrée d'une clé dans la base, pour keepassxc-cli et les messages.
pub fn entry_path(app: &AppSlug, role: KeyRole) -> String {
    format!("{GROUP}/{}", title(app, role))
}

/// Une base KeePassXC ouverte : keepassxc-cli, la base et son mot de passe, gardé en mémoire le
/// temps de la commande.
pub struct KeePassXc {
    cli: PathBuf,
    settings: Settings,
    password: String,
    /// Titres des entrées du groupe des clés, ou `None` tant que le groupe n'existe pas.
    titles: Option<Vec<String>>,
}

impl KeePassXc {
    /// Trouve keepassxc-cli, choisit la base, demande son mot de passe, et vérifie qu'il l'ouvre en
    /// lisant le groupe des clés. La base est alors retenue pour les fois suivantes.
    pub fn open(home: &Home, args: &VaultArgs, source: &mut dyn PasswordSource) -> Result<Self> {
        let cli = find_cli()?;
        let settings = args.settings(home)?;
        ensure!(
            settings.database.is_file(),
            "base KeePassXC {} introuvable",
            settings.database.display()
        );
        if let Some(key_file) = &settings.key_file {
            ensure!(
                key_file.is_file(),
                "fichier clé {} de la base KeePassXC introuvable",
                key_file.display()
            );
        }
        let password = source.read(&format!(
            "Mot de passe de la base KeePassXC {} : ",
            settings.database.display()
        ))?;
        let mut vault = Self {
            cli,
            settings,
            password,
            titles: None,
        };
        let root = vault.run("ls", &[], &[], None)?;
        if root
            .lines()
            .any(|line| line.trim_end() == format!("{GROUP}/"))
        {
            let titles = vault.run("ls", &[], &[GROUP.to_owned()], None)?;
            vault.titles = Some(titles.lines().map(str::to_owned).collect());
        }
        remember(home, &vault.settings)?;
        Ok(vault)
    }

    /// Le fichier de la base.
    pub fn database(&self) -> &Path {
        &self.settings.database
    }

    /// « KeePassXC (<base>) ».
    pub fn describe(&self) -> String {
        format!("KeePassXC ({})", self.settings.database.display())
    }

    pub fn has_entry(&self, app: &AppSlug, role: KeyRole) -> bool {
        let title = title(app, role);
        self.titles
            .as_ref()
            .is_some_and(|titles| titles.contains(&title))
    }

    /// L'entrée d'une clé, lue dans la base, ou `None` si elle n'y est pas. Son contenu est lu,
    /// pas encore vérifié : voir [`KeyEntry::check`].
    pub fn read(&self, app: &AppSlug, role: KeyRole) -> Result<Option<KeyEntry>> {
        if !self.has_entry(app, role) {
            return Ok(None);
        }
        let path = entry_path(app, role);
        let text = self.run(
            "show",
            &[
                "-s".to_owned(),
                "-a".to_owned(),
                "Password".to_owned(),
                "-a".to_owned(),
                "UserName".to_owned(),
                "-a".to_owned(),
                "Notes".to_owned(),
            ],
            std::slice::from_ref(&path),
            None,
        )?;
        let (password, rest) = text.split_once('\n').unwrap_or((&text, ""));
        let (username, notes) = rest.split_once('\n').unwrap_or((rest, ""));
        KeyEntry::parse(role, username, password, notes)
            .map(Some)
            .with_context(|| format!("entrée « {path} » de KeePassXC"))
    }

    /// Écrit ces entrées, qui ne doivent pas exister encore, dans le groupe des clés, créé s'il
    /// manque ; puis relit chacune, et vérifie que sa clé s'ouvre avec le mot de passe relu.
    pub fn save(&mut self, app: &AppSlug, entries: &[&KeyEntry]) -> Result<()> {
        for entry in entries {
            entry.check()?;
            ensure!(
                !self.has_entry(app, entry.role),
                "l'entrée « {} » existe déjà dans KeePassXC : je ne la remplace pas",
                entry_path(app, entry.role)
            );
        }
        if self.titles.is_none() {
            self.run("mkdir", &[], &[GROUP.to_owned()], None)?;
            self.titles = Some(Vec::new());
        }
        for entry in entries {
            let path = entry_path(app, entry.role);
            // Les notes passent en argument : elles n'ont que la clé secrète, chiffrée, et la clé
            // publique. Les mots de passe passent par l'entrée standard.
            self.run(
                "add",
                &[
                    "-p".to_owned(),
                    format!("--username={}", entry.key_id),
                    format!("--notes={}", entry.notes(app)),
                ],
                std::slice::from_ref(&path),
                Some(&entry.password),
            )?;
            if let Some(titles) = &mut self.titles {
                titles.push(title(app, entry.role));
            }
            let read = self
                .read(app, entry.role)?
                .with_context(|| format!("entrée « {path} » introuvable après son écriture"))?;
            ensure!(
                read == **entry,
                "l'entrée « {path} » relue diffère de ce qui a été écrit : vérifie-la dans KeePassXC"
            );
            read.check()
                .with_context(|| format!("entrée « {path} » relue"))?;
        }
        Ok(())
    }

    /// `keepassxc-cli <commande> -q [fichier clé, YubiKey] <options> <base> <arguments>`, avec le
    /// mot de passe de la base puis `then` sur l'entrée standard. Rend la sortie.
    fn run(
        &self,
        command: &str,
        options: &[String],
        arguments: &[String],
        then: Option<&str>,
    ) -> Result<String> {
        let mut process = Command::new(&self.cli);
        process.arg(command).arg("-q");
        if let Some(key_file) = &self.settings.key_file {
            process.arg("-k").arg(key_file);
        }
        if let Some(yubikey) = &self.settings.yubikey {
            process.arg("-y").arg(yubikey);
        }
        process
            .args(options)
            .arg(&self.settings.database)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process
            .spawn()
            .with_context(|| format!("lancement de {}", self.cli.display()))?;
        let mut input = format!("{}\n", self.password);
        if let Some(line) = then {
            input.push_str(line);
            input.push('\n');
        }
        if let Some(mut stdin) = child.stdin.take() {
            // keepassxc-cli peut s'arrêter avant d'avoir tout lu : son code de sortie le dira.
            let _ = stdin.write_all(input.as_bytes());
        }
        let output = child
            .wait_with_output()
            .with_context(|| format!("keepassxc-cli {command}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        // Avec -q, keepassxc-cli tait les échecs d'ouverture de la base, et eux seuls.
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if message.is_empty() {
            bail!(
                "KeePassXC n'a pas ouvert la base {} : mot de passe faux, ou fichier clé ou YubiKey manquant",
                self.settings.database.display()
            );
        }
        bail!("keepassxc-cli {command} : {message}")
    }
}

/// Ce que l'entrée d'une clé garde dans KeePassXC.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub role: KeyRole,
    pub key_id: KeyId,
    /// Le mot de passe qui ouvre la clé secrète.
    pub password: String,
    /// La clé secrète au format Tauri : le contenu du fichier `.key`.
    pub secret: String,
    /// La clé publique au format Tauri : le contenu du fichier `.key.pub`.
    pub public: String,
}

/// Sans le mot de passe ni la clé secrète.
impl fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyEntry({}, {})", self.role.label(), self.key_id)
    }
}

impl KeyEntry {
    /// Une clé et son mot de passe ; l'identifiant est celui de la clé publique.
    pub fn new(role: KeyRole, password: String, secret: &str, public: &str) -> Result<Self> {
        let key_id = PublicKey::from_tauri(public.trim())
            .context("clé publique illisible (format Tauri attendu)")?
            .key_id();
        Ok(Self {
            role,
            key_id,
            password,
            secret: secret.trim().to_owned(),
            public: public.trim().to_owned(),
        })
    }

    /// Vérifie que le mot de passe ouvre la clé secrète, et que c'est la paire de la clé publique.
    pub fn check(&self) -> Result<()> {
        let id = signing::secret_key_id(&self.secret, &self.password)
            .map_err(|_| anyhow!("le mot de passe n'ouvre pas la clé secrète"))?;
        ensure!(
            id == self.key_id,
            "la clé secrète {id} n'est pas celle de la clé publique {}",
            self.key_id
        );
        Ok(())
    }

    /// Les notes de l'entrée : de quoi refaire les fichiers à la main, sans l'outil.
    fn notes(&self, app: &AppSlug) -> String {
        let (role, file, restore) = match self.role {
            KeyRole::Main => (
                "Clé principale",
                format!("{app}.key"),
                format!("drakbase restore {app}"),
            ),
            KeyRole::Backup => (
                "Clé de secours",
                format!("{app}.backup.key"),
                format!("drakbase restore {app} --backup-key"),
            ),
        };
        format!(
            "{role} de l'app {app}, qui signe ses mises à jour pour DrakBase - Apps Market. Le mot de passe de cette entrée ouvre la clé secrète.\n\nClé secrète, contenu du fichier {file} :\n{}\n\nClé publique, contenu du fichier {file}.pub :\n{}\n\nPour la réinstaller sur un Mac : {restore}\n",
            self.secret, self.public
        )
    }

    /// Relit une entrée : les clés sont les lignes des notes qui en sont, où qu'elles soient.
    fn parse(role: KeyRole, username: &str, password: &str, notes: &str) -> Result<Self> {
        let lines = || notes.lines().map(str::trim);
        let public = lines()
            .find(|line| PublicKey::from_tauri(line).is_ok())
            .context("pas de clé publique dans les notes")?;
        let secret = lines()
            .find(|line| sign::is_secret_key(line))
            .context("pas de clé secrète dans les notes")?;
        let entry = Self::new(role, password.to_owned(), secret, public)?;
        ensure!(
            username.trim().eq_ignore_ascii_case(&entry.key_id.to_hex()),
            "nom d'utilisateur « {} », alors que la clé publique des notes est {}",
            username.trim(),
            entry.key_id
        );
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use am_protocol::signing::generate_keypair;

    use crate::password::Lines;

    /// Mot de passe des bases d'essai.
    const PASSWORD: &str = "base-essai-appsmarket";

    /// Une base KeePassXC jetable dans `dir`, rapide à ouvrir ; `None` sans keepassxc-cli, et
    /// l'essai s'arrête là.
    fn database(dir: &Path) -> Option<PathBuf> {
        let Ok(cli) = find_cli() else {
            eprintln!("keepassxc-cli introuvable : essai sauté");
            return None;
        };
        let path = dir.join("essai.kdbx");
        let mut child = Command::new(cli)
            .args(["db-create", "-q", "-p", "-t", "100"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{PASSWORD}\n{PASSWORD}\n").as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        Some(path)
    }

    fn slug(text: &str) -> AppSlug {
        text.parse().unwrap()
    }

    fn new_entry(role: KeyRole, password: &str) -> KeyEntry {
        let pair = generate_keypair(password).unwrap();
        KeyEntry::new(role, password.to_owned(), &pair.secret, &pair.public).unwrap()
    }

    fn open(home: &Home, database: &Path, password: &str) -> Result<KeePassXc> {
        KeePassXc::open(
            home,
            &VaultArgs::database(database),
            &mut Lines(format!("{password}\n").as_bytes()),
        )
    }

    #[test]
    fn entries_are_written_read_back_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let Some(database) = database(dir.path()) else {
            return;
        };
        let home = Home::at(&dir.path().join("outil"));
        let app = slug("demo");

        let error = format!("{:#}", open(&home, &database, "faux").err().unwrap());
        assert!(error.contains("mot de passe faux"), "{error}");
        assert!(!home.config_file().exists());

        let mut vault = open(&home, &database, PASSWORD).unwrap();
        assert!(!vault.has_entry(&app, KeyRole::Main));
        assert_eq!(vault.read(&app, KeyRole::Main).unwrap(), None);
        let main = new_entry(KeyRole::Main, "premier-mot-de-passe");
        let backup = new_entry(KeyRole::Backup, "second-mot-de-passe");
        vault.save(&app, &[&main, &backup]).unwrap();

        // Une autre ouverture, depuis la base retenue : les deux entrées s'y relisent.
        let vault = KeePassXc::open(
            &home,
            &VaultArgs::default(),
            &mut Lines(format!("{PASSWORD}\n").as_bytes()),
        )
        .unwrap();
        assert_eq!(vault.database(), database);
        assert_eq!(vault.read(&app, KeyRole::Main).unwrap(), Some(main.clone()));
        assert_eq!(vault.read(&app, KeyRole::Backup).unwrap(), Some(backup));
        assert!(!vault.has_entry(&slug("autre"), KeyRole::Main));

        // Une entrée n'est jamais remplacée, et une clé qui ne s'ouvre pas n'est pas écrite.
        let mut vault = vault;
        let error = format!("{:#}", vault.save(&app, &[&main]).unwrap_err());
        assert!(error.contains("existe déjà"), "{error}");
        let mut wrong = new_entry(KeyRole::Main, "troisième-mot-de-passe");
        wrong.password = "pas le bon".to_owned();
        let error = format!("{:#}", vault.save(&slug("autre"), &[&wrong]).unwrap_err());
        assert!(error.contains("n'ouvre pas"), "{error}");
        assert!(!vault.has_entry(&slug("autre"), KeyRole::Main));
    }

    #[test]
    fn notes_give_back_the_keys() {
        let app = slug("demo");
        let entry = new_entry(KeyRole::Backup, "mot-de-passe-de-secours");
        let notes = entry.notes(&app);
        assert!(notes.contains("demo.backup.key.pub"), "{notes}");
        assert!(
            notes.contains("drakbase restore demo --backup-key"),
            "{notes}"
        );
        let parsed = KeyEntry::parse(
            KeyRole::Backup,
            &entry.key_id.to_hex().to_lowercase(),
            &entry.password,
            &format!("Ajouté à la main.\n{notes}"),
        )
        .unwrap();
        assert_eq!(parsed, entry);
        parsed.check().unwrap();
        assert!(KeyEntry::parse(KeyRole::Backup, "0000000000000000", "x", &notes).is_err());
        assert!(KeyEntry::parse(KeyRole::Backup, "x", "x", "rien").is_err());
        assert_eq!(
            format!("{entry:?}"),
            format!("KeyEntry(clé de secours, {})", entry.key_id)
        );
    }

    #[test]
    fn the_last_database_of_keepassxc_is_proposed() {
        assert_eq!(
            parse_last_database("[General]\nLastDatabases=/a/b c.kdbx, /d/e.kdbx\n"),
            Some(PathBuf::from("/a/b c.kdbx"))
        );
        assert_eq!(parse_last_database("[General]\nLastDatabases=\n"), None);
        assert_eq!(parse_last_database("[GUI]\n"), None);
        assert_eq!(
            typed_path("/Users/moi/Mes\\ mots\\ de\\ passe.kdbx "),
            Some(PathBuf::from("/Users/moi/Mes mots de passe.kdbx"))
        );
        assert_eq!(typed_path("  "), None);
    }
}
