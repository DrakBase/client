//! Dossier de l'outil (`~/.appsmarket` par défaut) et fichiers de clés qu'il contient.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};

use crate::files;

/// Variable qui remplace `~/.appsmarket`, pour que les essais et les tests n'y écrivent jamais.
pub const HOME_VAR: &str = "APPSMARKET_HOME";

/// Identifiant d'une app (slug) : `[a-z0-9-]{1,64}`, comme dans les adresses du serveur
/// (docs/PROTOCOLE.md). Il nomme aussi les fichiers de clés, d'où ce contrôle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSlug(String);

impl FromStr for AppSlug {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if am_protocol::is_app_slug(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err("1 à 64 caractères parmi a-z, 0-9 et « - »".to_owned())
        }
    }
}

impl fmt::Display for AppSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Les deux clés d'une app : la principale signe les versions, la clé de secours ne sert que si
/// la principale est perdue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRole {
    Main,
    Backup,
}

impl KeyRole {
    pub const ALL: [KeyRole; 2] = [KeyRole::Main, KeyRole::Backup];

    pub fn label(self) -> &'static str {
        match self {
            KeyRole::Main => "clé principale",
            KeyRole::Backup => "clé de secours",
        }
    }
}

/// Fichiers d'une clé, nommés comme ceux de `tauri signer generate -w` : `<app>.key` et
/// `<app>.key.pub`, ou `<app>.backup.key` et `<app>.backup.key.pub`.
pub struct KeyFiles {
    pub secret: PathBuf,
    pub public: PathBuf,
}

/// Option `--home` des commandes qui lisent ou écrivent des clés.
#[derive(clap::Args)]
pub struct HomeArgs {
    /// Dossier de l'outil, qui contient keys/ [défaut : $APPSMARKET_HOME, sinon ~/.appsmarket]
    #[arg(long = "home", value_name = "DOSSIER")]
    dir: Option<PathBuf>,
}

impl HomeArgs {
    /// L'option `--home`, sinon `APPSMARKET_HOME`, sinon `~/.appsmarket`.
    pub fn resolve(&self) -> Result<Home> {
        choose(self.dir.clone(), env::var_os(HOME_VAR), env::home_dir()).map(Home)
    }
}

/// Dossier de l'outil.
pub struct Home(PathBuf);

impl Home {
    #[cfg(test)]
    pub fn at(dir: &std::path::Path) -> Self {
        Self(dir.to_owned())
    }

    pub fn keys_dir(&self) -> PathBuf {
        self.0.join("keys")
    }

    /// Adresse du serveur et jeton, quand ils sont rangés dans un fichier plutôt que dans le
    /// trousseau (`APPSMARKET_CREDENTIALS=file`, pour les essais).
    pub fn credentials_file(&self) -> PathBuf {
        self.0.join("credentials.json")
    }

    /// Mots de passe des clés, quand ils sont rangés dans un fichier plutôt que dans le trousseau
    /// (`APPSMARKET_CREDENTIALS=file`, pour les essais).
    pub fn key_passwords_file(&self) -> PathBuf {
        self.0.join("key-passwords.json")
    }

    /// Réglages retenus d'une fois sur l'autre : la base KeePassXC.
    pub fn config_file(&self) -> PathBuf {
        self.0.join("config.json")
    }

    /// Les sauvegardes vérifiées des clés, app par app.
    pub fn backups_file(&self) -> PathBuf {
        self.0.join("backups.json")
    }

    pub fn key_files(&self, app: &AppSlug, role: KeyRole) -> KeyFiles {
        let secret = self.keys_dir().join(match role {
            KeyRole::Main => format!("{app}.key"),
            KeyRole::Backup => format!("{app}.backup.key"),
        });
        let public = files::with_suffix(&secret, ".pub");
        KeyFiles { secret, public }
    }

    /// Crée le dossier des clés s'il manque, accessible à toi seul (0700).
    pub fn create_keys_dir(&self) -> Result<PathBuf> {
        let dir = self.keys_dir();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("création de {}", dir.display()))?;
        Ok(dir)
    }
}

fn choose(
    option: Option<PathBuf>,
    variable: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = option {
        return Ok(dir);
    }
    if let Some(dir) = variable.filter(|value| !value.is_empty()) {
        return Ok(dir.into());
    }
    user_home
        .map(|home| home.join(".appsmarket"))
        .with_context(|| {
            format!("dossier personnel introuvable : indique le dossier de l'outil avec --home ou {HOME_VAR}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_then_variable_then_user_home() {
        let user = Some(PathBuf::from("/Users/moi"));
        assert_eq!(
            choose(Some("/a".into()), Some("/b".into()), user.clone()).unwrap(),
            PathBuf::from("/a")
        );
        assert_eq!(
            choose(None, Some("/b".into()), user.clone()).unwrap(),
            PathBuf::from("/b")
        );
        assert_eq!(
            choose(None, Some("".into()), user.clone()).unwrap(),
            PathBuf::from("/Users/moi/.appsmarket")
        );
        assert!(choose(None, None, None).is_err());
    }

    #[test]
    fn key_files_follow_tauri_names() {
        let home = Home(PathBuf::from("/h"));
        let app: AppSlug = "translate".parse().unwrap();
        let main = home.key_files(&app, KeyRole::Main);
        assert_eq!(main.secret, PathBuf::from("/h/keys/translate.key"));
        assert_eq!(main.public, PathBuf::from("/h/keys/translate.key.pub"));
        let backup = home.key_files(&app, KeyRole::Backup);
        assert_eq!(backup.secret, PathBuf::from("/h/keys/translate.backup.key"));
        assert_eq!(
            backup.public,
            PathBuf::from("/h/keys/translate.backup.key.pub")
        );
    }

    #[test]
    fn slugs_cannot_escape_the_keys_dir() {
        for valid in ["translate", "rss-reader", "a", &"x".repeat(64)] {
            assert!(valid.parse::<AppSlug>().is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "../x",
            "a/b",
            "Translate",
            "mon app",
            "é",
            &"x".repeat(65),
        ] {
            assert!(invalid.parse::<AppSlug>().is_err(), "{invalid}");
        }
    }
}
