//! Emplacement des fichiers de l'agent sur la machine (docs/PROTOCOLE.md, « Agent
//! `drakbase-stack` »), réglables par des variables d'environnement pour les essais.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use am_protocol::Version;

/// Dossier de `config.json`.
pub const ENV_CONFIG_DIR: &str = "DRAKBASE_STACK_CONFIG_DIR";
/// Dossier des stacks (`<app>/state.json`, `releases/`, `.env`).
pub const ENV_STATE_DIR: &str = "DRAKBASE_STACK_STATE_DIR";
/// Dossier des unités systemd écrites par `auto on`.
pub const ENV_SYSTEMD_DIR: &str = "DRAKBASE_STACK_SYSTEMD_DIR";
/// Commande `docker` (un faux docker, dans les essais).
pub const ENV_DOCKER: &str = "DRAKBASE_STACK_DOCKER";
/// Commande `systemctl`.
pub const ENV_SYSTEMCTL: &str = "DRAKBASE_STACK_SYSTEMCTL";

/// Ceux que l'analyse du compose refuse de laisser monter.
const DEFAULT_CONFIG_DIR: &str = am_protocol::stack::compose::AGENT_CONFIG_DIR;
const DEFAULT_STATE_DIR: &str = am_protocol::stack::compose::AGENT_STATE_DIR;
const DEFAULT_SYSTEMD_DIR: &str = "/etc/systemd/system";

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub systemd_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        Self {
            config_dir: dir_from_env(ENV_CONFIG_DIR, DEFAULT_CONFIG_DIR),
            state_dir: dir_from_env(ENV_STATE_DIR, DEFAULT_STATE_DIR),
            systemd_dir: dir_from_env(ENV_SYSTEMD_DIR, DEFAULT_SYSTEMD_DIR),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn stack(&self, app: &str) -> StackPaths {
        StackPaths {
            dir: self.state_dir.join(app),
        }
    }
}

/// Dossier absolu : Docker Compose reçoit des chemins de projet, qui doivent l'être.
fn dir_from_env(name: &str, default: &str) -> PathBuf {
    let value = env::var_os(name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(default));
    std::path::absolute(&value).unwrap_or_else(|_| PathBuf::from(value))
}

/// Commande réglable par une variable d'environnement.
pub fn program_from_env(name: &str, default: &str) -> OsString {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(default))
}

/// Nom du fichier Compose de l'agent dans le dossier d'une version. L'archive ne peut pas en
/// contenir un du même nom : sa racine n'admet que quatre noms.
pub const CONTROL_COMPOSE_FILE: &str = "drakbase-control.json";

/// Fichiers d'une stack, sous `/var/lib/drakbase-stack/<app>/`.
#[derive(Clone, Debug)]
pub struct StackPaths {
    pub dir: PathBuf,
}

impl StackPaths {
    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.dir.join(".lock")
    }

    pub fn env_file(&self) -> PathBuf {
        self.dir.join(".env")
    }

    /// `.env` préparé pour la version qui arrive : il ne remplace `.env` qu'après le contrôle
    /// des permissions (étape 6).
    pub fn env_next(&self) -> PathBuf {
        self.dir.join(".env.next")
    }

    pub fn releases(&self) -> PathBuf {
        self.dir.join("releases")
    }

    pub fn release(&self, version: &Version) -> PathBuf {
        self.releases().join(version.to_string())
    }

    /// Où une version est extraite et examinée avant d'être mise en place (étapes 4 et 5) :
    /// `releases/.<version>.next`, à côté de son dossier. Les chemins que le compose résout hors
    /// du projet y sont donc ceux qu'il aura une fois en place.
    pub fn staging(&self, version: &Version) -> PathBuf {
        self.releases().join(format!(".{version}.next"))
    }

    /// Archive en cours de téléchargement (étape 2).
    pub fn partial(&self) -> PathBuf {
        self.releases().join(".partial")
    }

    pub fn compose_file(dir: &Path) -> PathBuf {
        dir.join("compose.yaml")
    }

    /// Fichier Compose de l'agent, dans le dossier d'une version : le montage du dossier de
    /// contrôle dans le service (docs/PROTOCOLE.md, « Mises à jour validées dans l'app »).
    pub fn control_compose_file(dir: &Path) -> PathBuf {
        dir.join(CONTROL_COMPOSE_FILE)
    }

    /// Dossier de contrôle, monté dans l'app : `status.json` et `request/`.
    pub fn control_dir(&self) -> PathBuf {
        self.dir.join("control")
    }

    pub fn status_file(&self) -> PathBuf {
        self.control_dir()
            .join(am_protocol::stack::control::STATUS_FILE)
    }

    /// Où `status.json` est préparé avant d'être mis en place : dans le dossier de la stack, sur
    /// le même volume, mais hors de `control/`, où l'app a pu écrire.
    pub fn status_temp(&self) -> PathBuf {
        self.dir.join(".status.json.tmp")
    }

    /// Les demandes que dépose l'app.
    pub fn request_dir(&self) -> PathBuf {
        self.control_dir()
            .join(am_protocol::stack::control::REQUEST_DIR)
    }

    /// Où `check` examine une version, sans l'installer : `.check/<version>`.
    pub fn check_root(&self) -> PathBuf {
        self.dir.join(".check")
    }

    pub fn check_dir(&self, version: &Version) -> PathBuf {
        self.check_root().join(version.to_string())
    }

    pub fn check_env(&self) -> PathBuf {
        self.dir.join(".env.check")
    }
}
