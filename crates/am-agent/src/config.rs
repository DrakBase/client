//! `/etc/drakbase-stack/config.json` : adresse du serveur, identifiant d'installation et
//! statistiques, communs à toutes les stacks de la machine et à `self-update`.

use std::fs;
use std::io;

use am_protocol::ServerUrl;
use anyhow::{Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::files;
use crate::paths::Paths;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Serveur de `self-update`, et celui d'une installation sans `--server`.
    pub server: ServerUrl,
    /// UUID v4 aléatoire, jamais dérivé du matériel (docs/PROTOCOLE.md, « Statistiques »).
    pub install_id: String,
    /// `false` après `install --no-stats` : l'identifiant n'est plus envoyé.
    #[serde(default = "enabled")]
    pub stats: bool,
}

fn enabled() -> bool {
    true
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.config_file();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("{} illisible", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("lecture de {}", path.display())),
        }
    }

    /// Lit la configuration, ou la crée avec `server` (sinon le serveur de production) et un
    /// nouvel identifiant d'installation.
    pub fn load_or_create(paths: &Paths, server: Option<&ServerUrl>) -> Result<Self> {
        if let Some(config) = Self::load(paths)? {
            return Ok(config);
        }
        let config = Self {
            server: server.cloned().unwrap_or_else(ServerUrl::production),
            install_id: new_install_id()?,
            stats: true,
        };
        config.save(paths)?;
        Ok(config)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        files::create_dir(&paths.config_dir)?;
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        files::replace(&paths.config_file(), text.as_bytes(), 0o644)
    }

    /// Identifiant à envoyer au serveur, sauf si les statistiques sont coupées.
    pub fn install_id(&self) -> Option<&str> {
        self.stats.then_some(self.install_id.as_str())
    }
}

/// UUID v4 : 122 bits aléatoires, version 4 et variante RFC 4122.
pub fn new_install_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("générateur aléatoire du système indisponible"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_ids_are_random_uuid_v4() {
        let first = new_install_id().unwrap();
        let second = new_install_id().unwrap();
        assert_ne!(first, second);
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
        assert!(first.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()));
    }

    #[test]
    fn created_once_then_kept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("etc"),
            state_dir: dir.path().join("lib"),
            systemd_dir: dir.path().join("systemd"),
        };
        let server = ServerUrl::parse("https://maj.example.fr").unwrap();
        let created = Config::load_or_create(&paths, Some(&server)).unwrap();
        assert_eq!(created.server, server);
        assert_eq!(created.install_id(), Some(created.install_id.as_str()));
        let other = ServerUrl::parse("https://autre.example.fr").unwrap();
        assert_eq!(
            Config::load_or_create(&paths, Some(&other)).unwrap(),
            created
        );
        let mut silent = created.clone();
        silent.stats = false;
        silent.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap().unwrap().install_id(), None);
    }
}
