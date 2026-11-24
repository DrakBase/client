//! Réponse du point de vérification, corps d'erreur et en-têtes du protocole.

use serde::{Deserialize, Serialize};

use crate::{Channel, Version};

/// Canal demandé par le client, si le paramètre `channel` est absent.
pub const HEADER_CHANNEL: &str = "x-appsmarket-channel";
/// Identifiant aléatoire de l'installation, pour les statistiques. Jamais dans l'URL.
pub const HEADER_INSTALL_ID: &str = "x-appsmarket-install-id";
/// Version de l'OS du client, si le paramètre `os_version` est absent.
pub const HEADER_OS_VERSION: &str = "x-appsmarket-os-version";
/// Envoi d'un artefact : contenu du `.sig` au format Tauri.
pub const HEADER_SIGNATURE: &str = "x-appsmarket-signature";
/// Envoi d'un artefact : nom de fichier proposé.
pub const HEADER_FILENAME: &str = "x-appsmarket-filename";
/// Envoi d'un artefact : SHA-256 attendu, en hexadécimal.
pub const HEADER_SHA256: &str = "x-appsmarket-sha256";

/// Corps de la réponse 200. `version`, `url` et `signature` sont exigés par `tauri-plugin-updater`,
/// qui ignore le reste.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResponse {
    pub version: Version,
    pub url: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// RFC 3339 en UTC.
    pub pub_date: String,
    pub size: u64,
    /// SHA-256 de l'artefact, en hexadécimal.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_os_version: Option<String>,
    pub channel: Channel,
}

/// Corps d'erreur commun à toutes les routes : un code stable et un message en français.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_optional_fields_are_omitted() {
        let response = UpdateResponse {
            version: Version::parse_release("1.2").unwrap(),
            url: "https://maj.example.fr/dl/x/a.app.tar.gz".to_owned(),
            signature: "c2ln".to_owned(),
            notes: None,
            pub_date: "2026-10-24T13:05:00Z".to_owned(),
            size: 10,
            sha256: "00".to_owned(),
            min_os_version: None,
            channel: Channel::Stable,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["version"], "1.2.0");
        assert!(json.get("notes").is_none());
        assert!(json.get("min_os_version").is_none());
    }
}
