//! Objets de l'API de publication (docs/API.md, « Publication par l'outil (jeton) ») : ce que le
//! serveur écrit et que l'outil lit, champ pour champ.
//!
//! - Un champ que le contrat dit facultatif (`?` dans docs/API.md) est écrit `null` quand il est
//!   absent, comme le fait le serveur, et se lit absent ou `null`. Seul `Release.images` n'est
//!   pas écrit du tout hors d'une stack.
//! - `Screenshot`, `Key`, `Release` et `Artifact` sont aussi ceux de l'administration.
//!   L'`App` de l'administration, elle, a d'autres champs (`listed`, `icon_url`, `members`…) :
//!   elle n'est pas ici. [`AppInfo`] est l'app telle que la voit un jeton.
//! - Le rôle et les portées restent des textes : un outil plus ancien doit pouvoir lire la
//!   réponse d'un serveur qui en connaît de nouveaux.

use serde::{Deserialize, Serialize};

use crate::{Arch, BundleType, Channel, Os, Version};

/// Longueur maximale des notes d'une version, en caractères.
pub const MAX_NOTES_CHARS: usize = 65_536;

/// Portée d'un jeton : créer une version, envoyer des artefacts, modifier un brouillon.
pub const SCOPE_RELEASES_WRITE: &str = "releases:write";
/// Portée d'un jeton : publier, retirer, changer le canal d'une version publiée.
pub const SCOPE_RELEASES_PUBLISH: &str = "releases:publish";
/// Portée d'un jeton : modifier la fiche publique (description et captures).
pub const SCOPE_LISTING_WRITE: &str = "listing:write";

crate::named_enum!(
    /// Technologie d'une app : le paquet Swift `DrakBase`, le plugin Tauri, une stack Docker
    /// (agent `drakbase-stack`) ou un exécutable Linux (`cli`, comme l'agent lui-même).
    pub enum AppKind, "le type d'app", {
        Swift => "swift",
        Tauri => "tauri",
        Stack => "stack",
        Cli => "cli",
    }
);

crate::named_enum!(
    /// État d'une clé de signature : une seule active par app, `next` pendant une rotation.
    pub enum KeyStatus, "l'état de clé", { Active => "active", Next => "next", Retired => "retired" }
);

crate::named_enum!(
    /// État d'une version. Un brouillon n'est jamais servi ; une version retirée sort des
    /// candidates et ses téléchargements renvoient 410.
    pub enum ReleaseStatus, "l'état de version", {
        Draft => "draft",
        Published => "published",
        Yanked => "yanked",
    }
);

/// `GET /api/v1/publish/whoami`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmI {
    pub user: WhoAmIUser,
    pub token: WhoAmIToken,
    /// Taille maximale d'un artefact (`AM_MAX_ARTIFACT_BYTES`), pour refuser tôt un fichier trop
    /// gros.
    pub max_artifact_bytes: u64,
    /// Adresse publique du serveur (`AM_PUBLIC_URL`), sans barre finale : celle des noms d'images
    /// d'une stack et de la commande d'installation.
    pub public_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmIUser {
    pub email: String,
    pub name: String,
    /// `superadmin` ou `editor`.
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmIToken {
    pub name: String,
    /// Voir `SCOPE_RELEASES_WRITE`, `SCOPE_RELEASES_PUBLISH` et `SCOPE_LISTING_WRITE`.
    pub scopes: Vec<String>,
    /// Slug de la seule app du jeton, s'il en a une.
    #[serde(default)]
    pub app: Option<String>,
    /// RFC 3339 UTC.
    pub expires_at: String,
}

impl WhoAmIToken {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }
}

/// `GET` et `PATCH /api/v1/publish/apps/{app}` : l'app telle que la voit un jeton.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub slug: String,
    pub name: String,
    pub kind: AppKind,
    #[serde(default)]
    pub bundle_id: Option<String>,
    pub description: String,
    /// Dans l'ordre d'affichage ; l'outil les compare par leur `sha256`.
    pub screenshots: Vec<Screenshot>,
    pub keys: Vec<Key>,
    pub latest: Latest,
}

/// Plus hautes versions publiées de chaque canal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    #[serde(default)]
    pub stable: Option<Version>,
    #[serde(default)]
    pub beta: Option<Version>,
}

impl Latest {
    pub fn of(&self, channel: Channel) -> Option<&Version> {
        match channel {
            Channel::Stable => self.stable.as_ref(),
            Channel::Beta => self.beta.as_ref(),
        }
    }
}

/// `Key` : une clé de signature de l'app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// Identifiant minisign, 16 chiffres hexadécimaux.
    pub key_id: String,
    /// Au format Tauri.
    pub public_key: String,
    pub status: KeyStatus,
    /// RFC 3339 UTC.
    pub created_at: String,
}

/// `Screenshot` : une capture d'écran de la fiche publique.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    /// ULID.
    pub id: String,
    /// L'image pour l'administration, que le jeton ne lit pas.
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    /// SHA-256 en hexadécimal minuscule.
    pub sha256: String,
    pub caption: String,
    /// RFC 3339 UTC.
    pub created_at: String,
}

/// Corps de `POST /api/v1/publish/apps/{app}/releases`. Un champ absent ou `null` garde la
/// valeur d'un brouillon existant, ou prend celle par défaut d'un nouveau (canal `stable`, notes
/// vides, aucune clé déclarée) ; un champ présent la remplace, même vide.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRelease {
    /// Lue par le serveur avec les règles d'une version publiée (`Version::parse_release`) : un
    /// texte, pour que le refus soit le sien.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_md: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_key_ids: Option<Vec<String>>,
}

/// `Release` : une version et ses artefacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: Version,
    pub channel: Channel,
    pub status: ReleaseStatus,
    pub notes_md: String,
    /// Identifiants (16 chiffres hexadécimaux) des clés que cette version embarque.
    pub trusted_key_ids: Vec<String>,
    #[serde(default)]
    pub created_by: Option<Creator>,
    /// RFC 3339 UTC, comme les trois dates qui suivent.
    pub created_at: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub yanked_at: Option<String>,
    #[serde(default)]
    pub yank_reason: Option<String>,
    pub artifacts: Vec<Artifact>,
    /// Version d'une stack seulement : ses images, relevées à l'envoi de son archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ReleaseImage>>,
}

/// Le compte qui a créé une version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Creator {
    pub id: i64,
    pub name: String,
    pub email: String,
}

/// Une image d'une version de stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseImage {
    pub image: String,
    /// Empreinte de l'index ou du manifeste déclaré dans `stack.json`.
    pub digest: String,
    pub platforms: Vec<ImagePlatform>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePlatform {
    /// Plateforme Docker : `linux/arm64`.
    pub platform: String,
    /// Taille de la configuration et des couches de cette plateforme.
    pub size: u64,
}

/// `Artifact` : un fichier d'une version, pour un OS, une arch et un type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// ULID.
    pub id: String,
    pub os: Os,
    pub arch: Arch,
    pub bundle_type: BundleType,
    /// Nom réduit (`names::sanitize_filename`).
    pub filename: String,
    pub size: u64,
    /// SHA-256 en hexadécimal.
    pub sha256: String,
    #[serde(default)]
    pub signed_key_id: Option<String>,
    #[serde(default)]
    pub signed_version: Option<String>,
    #[serde(default)]
    pub min_os_version: Option<String>,
    /// Adresse de téléchargement ; celle d'un brouillon demande une session d'administration.
    pub url: String,
    /// RFC 3339 UTC.
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    /// Lit le JSON tel que le serveur l'écrit, puis le réécrit : il doit revenir à l'identique.
    fn round_trip<T: Serialize + DeserializeOwned>(written: Value) -> T {
        let read: T = serde_json::from_value(written.clone()).unwrap();
        assert_eq!(serde_json::to_value(&read).unwrap(), written);
        read
    }

    #[test]
    fn whoami() {
        let whoami: WhoAmI = round_trip(json!({
            "user": { "email": "dev@example.fr", "name": "Dev", "role": "editor" },
            "token": {
                "name": "portable", "scopes": ["releases:write", "listing:write"],
                "app": null, "expires_at": "2027-01-01T00:00:00Z"
            },
            "max_artifact_bytes": 2147483648u64,
            "public_url": "https://maj.example.fr"
        }));
        assert!(whoami.token.has_scope(SCOPE_RELEASES_WRITE));
        assert!(!whoami.token.has_scope(SCOPE_RELEASES_PUBLISH));
        assert_eq!(whoami.token.app, None);
        // Une portée ou un rôle inconnus de cette version se lisent quand même.
        let newer = json!({
            "user": { "email": "a@b.fr", "name": "", "role": "auditor" },
            "token": { "name": "x", "scopes": ["stats:read"], "app": "demo", "expires_at": "" },
            "max_artifact_bytes": 1, "public_url": "https://maj.example.fr"
        });
        assert!(serde_json::from_value::<WhoAmI>(newer).is_ok());
    }

    #[test]
    fn app_info() {
        let app: AppInfo = round_trip(json!({
            "slug": "demo", "name": "Démo", "kind": "stack", "bundle_id": null,
            "description": "Une stack.",
            "screenshots": [{
                "id": "01JB8Z6T3KQ4N5V7W9X0Y1Z2A3",
                "url": "https://maj.example.fr/api/admin/apps/demo/screenshots/01JB8Z6T3KQ4N5V7W9X0Y1Z2A3",
                "width": 1280, "height": 800, "size": 34567, "sha256": "9f2c", "caption": "",
                "created_at": "2026-10-24T13:05:00Z"
            }],
            "keys": [{
                "key_id": "7187B435F3714F09", "public_key": "dW50cnVzdGVk", "status": "active",
                "created_at": "2026-10-24T13:05:00Z"
            }],
            "latest": { "stable": "1.2.0", "beta": null }
        }));
        assert_eq!(app.kind, AppKind::Stack);
        assert_eq!(app.keys[0].status, KeyStatus::Active);
        assert_eq!(app.latest.of(Channel::Stable).unwrap().to_string(), "1.2.0");
        assert_eq!(app.latest.of(Channel::Beta), None);
        // Un type d'app inconnu est refusé : l'outil ne saurait pas quoi publier.
        assert!(serde_json::from_value::<AppKind>(json!("flutter")).is_err());
        // Les champs facultatifs peuvent aussi manquer.
        let bare = json!({
            "slug": "demo", "name": "Démo", "kind": "swift", "description": "",
            "screenshots": [], "keys": [], "latest": {}
        });
        let bare: AppInfo = serde_json::from_value(bare).unwrap();
        assert_eq!((bare.bundle_id, bare.latest.stable), (None, None));
    }

    #[test]
    fn release_and_artifact() {
        let artifact = json!({
            "id": "01JB8Z6T3KQ4N5V7W9X0Y1Z2A3", "os": "darwin", "arch": "universal",
            "bundle_type": "app", "filename": "Translate-1.2.0.app.tar.gz", "size": 734243,
            "sha256": "9f2c", "signed_key_id": "7187B435F3714F09", "signed_version": "1.2.0",
            "min_os_version": null,
            "url": "https://maj.example.fr/dl/01JB8Z6T3KQ4N5V7W9X0Y1Z2A3/Translate-1.2.0.app.tar.gz",
            "created_at": "2026-10-24T13:05:00Z"
        });
        let release: Release = round_trip(json!({
            "version": "1.2.0", "channel": "beta", "status": "draft", "notes_md": "- Notes",
            "trusted_key_ids": ["7187B435F3714F09"],
            "created_by": { "id": 3, "name": "Dev", "email": "dev@example.fr" },
            "created_at": "2026-10-24T13:05:00Z", "published_at": null, "yanked_at": null,
            "yank_reason": null, "artifacts": [artifact]
        }));
        assert_eq!(release.status, ReleaseStatus::Draft);
        assert_eq!(release.artifacts[0].arch, Arch::Universal);
        assert_eq!(release.images, None);

        // Une version de stack porte ses images.
        let stack: Release = round_trip(json!({
            "version": "0.2.0", "channel": "stable", "status": "published", "notes_md": "",
            "trusted_key_ids": [], "created_by": null, "created_at": "2026-11-12T16:00:00Z",
            "published_at": "2026-11-12T16:00:05Z", "yanked_at": null, "yank_reason": null,
            "artifacts": [],
            "images": [{
                "image": "home-manager", "digest": "sha256:d812",
                "platforms": [{ "platform": "linux/arm64", "size": 27654321 }]
            }]
        }));
        assert_eq!(
            stack.images.unwrap()[0].platforms[0].platform,
            "linux/arm64"
        );
    }

    #[test]
    fn new_release() {
        // Ce qu'écrit l'outil : les champs absents ne sont pas écrits.
        let minimal = NewRelease {
            version: "1.2.0".to_owned(),
            ..NewRelease::default()
        };
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            json!({ "version": "1.2.0" })
        );
        let full: NewRelease = round_trip(json!({
            "version": "1.2.0", "channel": "beta", "notes_md": "", "trusted_key_ids": []
        }));
        assert_eq!(full.channel, Some(Channel::Beta));
        assert_eq!(full.notes_md.as_deref(), Some(""));
        // Ce que lit le serveur : `null` vaut absent.
        let nulls = json!({
            "version": "v1.2", "channel": null, "notes_md": null, "trusted_key_ids": null
        });
        let nulls: NewRelease = serde_json::from_value(nulls).unwrap();
        assert_eq!(
            nulls,
            NewRelease {
                version: "v1.2".to_owned(),
                ..NewRelease::default()
            }
        );
    }

    #[test]
    fn names_of_the_enums() {
        assert_eq!(AppKind::Cli.as_str(), "cli");
        assert_eq!("retired".parse::<KeyStatus>().unwrap(), KeyStatus::Retired);
        assert_eq!(ReleaseStatus::Yanked.to_string(), "yanked");
        assert_eq!(
            "archived".parse::<ReleaseStatus>().unwrap_err().to_string(),
            "valeur « archived » inconnue pour l'état de version"
        );
    }
}
