//! `stack.json` : description d'une stack (docs/PROTOCOLE.md, « `stack.json` »). Deux lectures :
//! celle du projet (`parse_source`, par l'outil) et celle de l'archive publiée (`parse_published`,
//! par le serveur et l'agent).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::{
    Digest, StackError, docker_platform, is_app_slug, is_image_name, is_service_name,
    parse_docker_platform,
};
use crate::os_version::OsVersion;
use crate::platform::Arch;
use crate::signature::{KeyId, PublicKey};
use crate::version::Version;

pub const FORMAT: u64 = 1;
pub(crate) const DEFAULT_HEALTH_TIMEOUT: u32 = 120;
pub(crate) const HEALTH_TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 10..=900;
/// Plateformes construites par défaut : arm64 (Raspberry Pi, Mac) puis amd64.
pub const DEFAULT_PLATFORMS: [Arch; 2] = [Arch::Aarch64, Arch::X86_64];

const FIELDS: [&str; 12] = [
    "format",
    "app",
    "version",
    "service",
    "health_timeout",
    "rollback",
    "pre_update",
    "min_compose_version",
    "platforms",
    "public_keys",
    "images",
    "app_control",
];
/// Champs que remplit l'outil : refusés dans le `stack.json` du projet, obligatoires une fois publié.
const TOOL_FIELDS: [&str; 3] = ["app", "version", "images"];

/// Conduite de l'agent quand la nouvelle version ne devient pas saine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rollback {
    /// Retour seul à la version précédente.
    Safe,
    /// Arrêt et explication : une nouvelle version peut avoir changé le format des données.
    #[default]
    Manual,
}

impl Rollback {
    pub const fn as_str(self) -> &'static str {
        match self {
            Rollback::Safe => "safe",
            Rollback::Manual => "manual",
        }
    }
}

/// Contenu validé d'un `stack.json`. Dans la lecture du projet, `app`, `version` et `images`
/// sont vides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackManifest {
    pub app: Option<String>,
    pub version: Option<Version>,
    pub service: String,
    pub health_timeout: u32,
    pub rollback: Rollback,
    pub pre_update: Option<String>,
    pub min_compose_version: Option<String>,
    /// Chemin, dans le conteneur de `service`, où l'agent monte le dossier de contrôle des
    /// mises à jour validées dans l'app.
    pub app_control: Option<String>,
    pub platforms: Vec<Arch>,
    /// Clés publiques au format Tauri, telles qu'écrites.
    pub public_keys: Vec<String>,
    pub images: BTreeMap<String, Digest>,
}

impl StackManifest {
    /// Lit le `stack.json` du projet. Les champs remplis par l'outil y sont refusés.
    pub fn parse_source(bytes: &[u8]) -> Result<Self, StackError> {
        let object = parse_object(bytes)?;
        if let Some(field) = TOOL_FIELDS
            .iter()
            .find(|field| object.contains_key(**field))
        {
            return Err(StackError::FieldToolOnly((*field).to_owned()));
        }
        parse_common(&object)
    }

    /// Lit le `stack.json` d'une archive publiée, qui doit être celle de `app` en `version`.
    pub fn parse_published(bytes: &[u8], app: &str, version: &Version) -> Result<Self, StackError> {
        let object = parse_object(bytes)?;
        for field in TOOL_FIELDS {
            if !object.contains_key(field) {
                return Err(StackError::FieldMissing(field.to_owned()));
            }
        }
        let mut manifest = parse_common(&object)?;

        let found_app = string_field(&object, "app")?.unwrap_or_default();
        if !is_app_slug(&found_app) {
            return Err(StackError::AppInvalid(found_app));
        }
        if found_app != app {
            return Err(StackError::AppMismatch {
                expected: app.to_owned(),
                found: found_app,
            });
        }
        let raw_version = string_field(&object, "version")?.unwrap_or_default();
        let found_version = Version::parse_release(&raw_version)
            .ok()
            .filter(|parsed| parsed.to_string() == raw_version)
            .ok_or_else(|| StackError::VersionInvalid(raw_version.clone()))?;
        if &found_version != version {
            return Err(StackError::VersionMismatch {
                expected: version.to_string(),
                found: raw_version,
            });
        }
        manifest.images = parse_images(&object["images"])?;
        manifest.app = Some(found_app);
        manifest.version = Some(found_version);
        Ok(manifest)
    }

    /// Version publiée, faite par l'outil à partir du `stack.json` du projet.
    pub fn published(
        mut self,
        app: &str,
        version: &Version,
        images: BTreeMap<String, Digest>,
    ) -> Self {
        self.app = Some(app.to_owned());
        self.version = Some(version.clone());
        self.images = images;
        self
    }

    /// Clés publiques lues ; elles ont été validées à la lecture.
    pub fn keys(&self) -> Vec<PublicKey> {
        self.public_keys
            .iter()
            .filter_map(|key| PublicKey::from_tauri(key).ok())
            .collect()
    }

    /// Identifiants des clés : les `trusted_key_ids` de la version.
    pub fn trusted_key_ids(&self) -> Vec<KeyId> {
        self.keys().iter().map(PublicKey::key_id).collect()
    }

    /// Texte JSON, tous les champs écrits, clés dans l'ordre alphabétique, suivi d'un saut de ligne.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("format".into(), json!(FORMAT));
        if let Some(app) = &self.app {
            object.insert("app".into(), json!(app));
        }
        if let Some(version) = &self.version {
            object.insert("version".into(), json!(version.to_string()));
        }
        object.insert("service".into(), json!(self.service));
        object.insert("health_timeout".into(), json!(self.health_timeout));
        object.insert("rollback".into(), json!(self.rollback.as_str()));
        if let Some(pre_update) = &self.pre_update {
            object.insert("pre_update".into(), json!(pre_update));
        }
        if let Some(version) = &self.min_compose_version {
            object.insert("min_compose_version".into(), json!(version));
        }
        let platforms: Vec<&str> = self
            .platforms
            .iter()
            .filter_map(|arch| docker_platform(*arch))
            .collect();
        object.insert("platforms".into(), json!(platforms));
        object.insert("public_keys".into(), json!(self.public_keys));
        if let Some(path) = &self.app_control {
            object.insert("app_control".into(), json!(path));
        }
        if self.app.is_some() {
            let images: Map<String, Value> = self
                .images
                .iter()
                .map(|(name, digest)| (name.clone(), json!(digest.as_str())))
                .collect();
            object.insert("images".into(), Value::Object(images));
        }
        let mut text = serde_json::to_string_pretty(&Value::Object(object)).unwrap_or_default();
        text.push('\n');
        text
    }
}

fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, StackError> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => {
            if let Some(unknown) = object.keys().find(|key| !FIELDS.contains(&key.as_str())) {
                return Err(StackError::FieldUnknown(unknown.clone()));
            }
            Ok(object)
        }
        Ok(_) => Err(StackError::JsonInvalid("un objet JSON est attendu".into())),
        Err(error) => Err(StackError::JsonInvalid(error.to_string())),
    }
}

fn string_field(object: &Map<String, Value>, name: &str) -> Result<Option<String>, StackError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(StackError::FieldType(name.to_owned())),
    }
}

fn required_string(object: &Map<String, Value>, name: &str) -> Result<String, StackError> {
    string_field(object, name)?.ok_or_else(|| StackError::FieldMissing(name.to_owned()))
}

fn string_list(object: &Map<String, Value>, name: &str) -> Result<Option<Vec<String>>, StackError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| StackError::FieldType(name.to_owned())),
        Some(_) => Err(StackError::FieldType(name.to_owned())),
    }
}

/// Champs communs aux deux lectures, dans l'ordre du tableau de docs/PROTOCOLE.md.
fn parse_common(object: &Map<String, Value>) -> Result<StackManifest, StackError> {
    match object.get("format") {
        None => return Err(StackError::FieldMissing("format".into())),
        Some(Value::Number(number)) => {
            if number.as_u64() != Some(FORMAT) {
                return Err(StackError::FormatUnsupported(number.to_string()));
            }
        }
        Some(_) => return Err(StackError::FieldType("format".into())),
    }

    let service = required_string(object, "service")?;
    if !is_service_name(&service) {
        return Err(StackError::ServiceNameInvalid(service));
    }

    let health_timeout = match object.get("health_timeout") {
        None => DEFAULT_HEALTH_TIMEOUT,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(seconds) if HEALTH_TIMEOUT_RANGE.contains(&seconds) => seconds as u32,
            _ => return Err(StackError::HealthTimeoutOutOfRange(number.to_string())),
        },
        Some(_) => return Err(StackError::FieldType("health_timeout".into())),
    };

    let rollback = match string_field(object, "rollback")?.as_deref() {
        None | Some("manual") => Rollback::Manual,
        Some("safe") => Rollback::Safe,
        Some(other) => return Err(StackError::RollbackInvalid(other.to_owned())),
    };

    let pre_update = string_field(object, "pre_update")?;
    if let Some(name) = &pre_update {
        if !is_service_name(name) {
            return Err(StackError::ServiceNameInvalid(name.clone()));
        }
        if *name == service {
            return Err(StackError::PreUpdateSameAsService);
        }
    }

    let app_control = string_field(object, "app_control")?;
    if let Some(path) = &app_control
        && !is_container_path(path)
    {
        return Err(StackError::AppControlInvalid(path.clone()));
    }

    let min_compose_version = string_field(object, "min_compose_version")?;
    if let Some(version) = &min_compose_version
        && OsVersion::parse_exact(version).is_none()
    {
        return Err(StackError::ComposeVersionInvalid(version.clone()));
    }

    let platforms = match string_list(object, "platforms")? {
        None => DEFAULT_PLATFORMS.to_vec(),
        Some(names) => {
            if names.is_empty() || names.len() > 5 {
                return Err(StackError::PlatformsCount);
            }
            let mut platforms = Vec::with_capacity(names.len());
            for name in names {
                let arch = parse_docker_platform(&name)?;
                if platforms.contains(&arch) {
                    return Err(StackError::PlatformDuplicate(name));
                }
                platforms.push(arch);
            }
            platforms
        }
    };

    let public_keys = string_list(object, "public_keys")?
        .ok_or_else(|| StackError::FieldMissing("public_keys".into()))?;
    if public_keys.is_empty() || public_keys.len() > 4 {
        return Err(StackError::PublicKeysCount);
    }
    let mut key_ids = BTreeSet::new(); // identifiants en hexadécimal
    for (index, key) in public_keys.iter().enumerate() {
        let parsed =
            PublicKey::from_tauri(key).map_err(|_| StackError::PublicKeyInvalid(index + 1))?;
        if !key_ids.insert(parsed.key_id().to_hex()) {
            return Err(StackError::PublicKeyDuplicate(parsed.key_id().to_hex()));
        }
    }

    Ok(StackManifest {
        app: None,
        version: None,
        service,
        health_timeout,
        rollback,
        pre_update,
        min_compose_version,
        app_control,
        platforms,
        public_keys,
        images: BTreeMap::new(),
    })
}

fn parse_images(value: &Value) -> Result<BTreeMap<String, Digest>, StackError> {
    let Value::Object(entries) = value else {
        return Err(StackError::FieldType("images".into()));
    };
    if entries.is_empty() || entries.len() > 16 {
        return Err(StackError::ImagesCount);
    }
    let mut images = BTreeMap::new();
    for (name, digest) in entries {
        if !is_image_name(name) {
            return Err(StackError::ImageNameInvalid(name.clone()));
        }
        let Value::String(digest) = digest else {
            return Err(StackError::FieldType("images".into()));
        };
        images.insert(name.clone(), Digest::parse(digest)?);
    }
    Ok(images)
}

/// Chemin absolu dans un conteneur : pas `/` seul, 255 caractères au plus, composants ordinaires.
fn is_container_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty()
        && path.len() <= 255
        && rest.split('/').all(|part| {
            part != "."
                    && part != ".."
                    && !part.is_empty()
                    // L'agent écrit ce chemin dans son propre fichier Compose : ni `$`, que
                    // Compose interpolerait, ni caractère caché.
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}
