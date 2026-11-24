//! Stacks Docker (docs/PROTOCOLE.md, « Stacks Docker ») : empreintes, noms, plateformes Docker,
//! `stack.json`, archive `stack.tar.gz`, analyse du compose résolu et lecture des manifestes OCI.
//! Partagé par le serveur, l'outil et l'agent ; aucune entrée/sortie ici.

pub mod archive;
pub mod compose;
pub mod control;
pub mod manifest;
pub mod oci;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

pub use crate::names::is_app_slug;
use crate::platform::Arch;

/// Profil Compose du service `pre_update` : `up` ne le démarre pas.
pub const HOOKS_PROFILE: &str = "drakbase-hooks";

/// Toute règle enfreinte, avec un code stable (celui des jeux d'essai de `fixtures/stack/`) et un
/// message en français.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StackError {
    // --- noms et empreintes ---
    #[error("empreinte « {0} » illisible : sha256: suivi de 64 caractères hexadécimaux minuscules")]
    DigestInvalid(String),
    #[error("nom d'image « {0} » invalide")]
    ImageNameInvalid(String),
    #[error("nom de service « {0} » invalide")]
    ServiceNameInvalid(String),
    #[error("plateforme Docker « {0} » non prise en charge")]
    PlatformInvalid(String),

    // --- stack.json ---
    #[error("stack.json illisible : {0}")]
    JsonInvalid(String),
    #[error("stack.json : champ inconnu « {0} »")]
    FieldUnknown(String),
    #[error("stack.json : champ « {0} » obligatoire")]
    FieldMissing(String),
    #[error("stack.json : champ « {0} » d'un mauvais type")]
    FieldType(String),
    #[error("stack.json du projet : le champ « {0} » est rempli par l'outil")]
    FieldToolOnly(String),
    #[error("stack.json : format {0} inconnu")]
    FormatUnsupported(String),
    #[error("stack.json : app « {0} » invalide")]
    AppInvalid(String),
    #[error("stack.json : app « {found} » au lieu de « {expected} »")]
    AppMismatch { expected: String, found: String },
    #[error("stack.json : version « {0} » non canonique")]
    VersionInvalid(String),
    #[error("stack.json : version « {found} » au lieu de « {expected} »")]
    VersionMismatch { expected: String, found: String },
    #[error("stack.json : health_timeout {0} hors de 10 à 900")]
    HealthTimeoutOutOfRange(String),
    #[error("stack.json : rollback « {0} » inconnu (safe ou manual)")]
    RollbackInvalid(String),
    #[error("stack.json : pre_update ne peut pas être le service surveillé")]
    PreUpdateSameAsService,
    #[error("stack.json : version de Docker Compose « {0} » illisible")]
    ComposeVersionInvalid(String),
    #[error("stack.json : de 1 à 5 plateformes")]
    PlatformsCount,
    #[error("stack.json : plateforme « {0} » en double")]
    PlatformDuplicate(String),
    #[error("stack.json : de 1 à 4 clés publiques")]
    PublicKeysCount,
    #[error("stack.json : clé publique n° {0} illisible")]
    PublicKeyInvalid(usize),
    #[error("stack.json : clé {0} en double")]
    PublicKeyDuplicate(String),
    #[error("stack.json : de 1 à 16 images")]
    ImagesCount,
    #[error("stack.json : app_control « {0} » n'est pas un chemin absolu ordinaire")]
    AppControlInvalid(String),

    // --- demandes de l'app (dossier de contrôle) ---
    #[error("demande illisible : {0}")]
    RequestInvalid(String),
    #[error("demande de plus de 4 Kio")]
    RequestTooLarge,
    #[error("demande au format {0} inconnu")]
    RequestFormat(String),
    #[error("demande « {0} » inconnue (check ou update)")]
    RequestAction(String),
    #[error("demande d'une version « {0} » non canonique")]
    RequestVersion(String),

    // --- archive ---
    #[error("archive illisible : {0}")]
    ArchiveInvalid(String),
    #[error("archive trop grosse une fois décompressée (16 Mio au plus)")]
    ArchiveTooLarge,
    #[error("archive : plus de 1000 entrées")]
    ArchiveTooManyEntries,
    #[error("archive : « {0} » n'est ni un fichier ni un dossier")]
    EntryType(String),
    #[error("archive : chemin « {0} » refusé")]
    EntryPath(String),
    #[error("archive : « {0} » n'a pas sa place dans une stack")]
    EntryUnexpected(String),
    #[error("archive : « {0} » apparaît deux fois")]
    EntryDuplicate(String),
    #[error("archive : « {0} » manque")]
    EntryMissing(String),

    // --- compose ---
    #[error("configuration Compose illisible : {0}")]
    ComposeJsonInvalid(String),
    #[error("le service surveillé « {0} » n'existe pas dans le compose")]
    ServiceMissing(String),
    #[error("le service pre_update « {0} » n'existe pas dans le compose")]
    PreUpdateMissing(String),
    #[error("service « {0} » : pas d'image")]
    ImageMissing(String),
    #[error("service « {0} » : une section build ne se publie pas")]
    BuildPresent(String),
    #[error("service « {service} » : image « {image} » non épinglée par empreinte (@sha256:…)")]
    ImageNotPinned { service: String, image: String },
    #[error("service « {service} » : image « {image} » mal écrite (<hôte>/<app>/<image>@sha256:…)")]
    ImageMalformed { service: String, image: String },
    #[error("service « {service} » : image « {image} » d'une autre app")]
    ImageOtherApp { service: String, image: String },
    #[error(
        "service « {service} » : image « {image} » absente de stack.json ou d'une autre empreinte"
    )]
    ImageNotDeclared { service: String, image: String },
    #[error("image « {0} » de stack.json utilisée par aucun service")]
    ImageUnused(String),
    #[error("service « {0} » : seul pre_update peut avoir un profil")]
    ProfileUnexpected(String),
    #[error("service pre_update « {0} » : son seul profil doit être drakbase-hooks")]
    PreUpdateProfile(String),
    #[error("« {0} » : un fichier de la stack doit être dans files/")]
    BindOutsideFiles(String),
    #[error("« {0} » : un fichier de la stack se monte en lecture seule")]
    BindNotReadOnly(String),
    #[error("« {0} » : un chemin monté ne contient ni composant « . » ni composant « .. »")]
    BindPathInvalid(String),
    #[error("compose : « {0} » n'est pas pris en charge dans une stack")]
    ComposeFieldUnsupported(String),
    #[error("compose : « {field} » vaut « {value} », qui n'est pas pris en charge dans une stack")]
    ComposeValueUnsupported { field: String, value: String },
    /// Le champ et la valeur sont déjà rendus visibles (`crate::visible`).
    #[error(
        "compose : « {field} » vaut « {value} », avec un caractère de contrôle, de sens d'écriture ou invisible"
    )]
    ComposeTextHidden { field: String, value: String },
    #[error("« {0} » : un env_file ou un label_file se lit dans files/, par un chemin relatif")]
    EnvFileOutsideFiles(String),
    #[error("compose.yaml de l'archive : ce n'est pas le JSON qu'écrit l'outil ({0})")]
    ComposeNotJson(String),
    #[error(
        ".env.example : « {0} » est un réglage de Docker ou de Docker Compose, pas de la stack"
    )]
    EnvNameReserved(String),
    #[error(
        ".env.example, ligne {line} : seuls sont admis une ligne vide, un commentaire (#) et NOM=valeur, la valeur tenant sur la ligne"
    )]
    EnvLineUnsupported { line: usize },

    // --- manifestes OCI ---
    #[error("manifeste illisible : {0}")]
    OciJsonInvalid(String),
    #[error("type de manifeste « {0} » non pris en charge")]
    OciMediaType(String),
    #[error("le manifeste se dit « {found} », envoyé comme « {declared} »")]
    OciMediaTypeMismatch { declared: String, found: String },
    #[error("manifeste : schemaVersion doit valoir 2")]
    OciSchema,
    #[error("type de couche « {0} » non pris en charge")]
    OciLayerType(String),
    #[error("couche « {0} » hébergée ailleurs ou à distribution restreinte")]
    OciForeignLayer(String),
    #[error("plateforme « {0} » non prise en charge (attestation ?)")]
    OciPlatform(String),
    #[error("plateforme « {0} » en double dans l'index")]
    OciPlatformDuplicate(String),
    #[error("l'index désigne « {0} », qui n'est pas un manifeste d'image")]
    OciChildType(String),
}

impl StackError {
    /// Code stable de la règle enfreinte, tel qu'il figure dans les jeux d'essai.
    pub const fn code(&self) -> &'static str {
        match self {
            StackError::DigestInvalid(_) => "digest_invalid",
            StackError::ImageNameInvalid(_) => "image_name_invalid",
            StackError::ServiceNameInvalid(_) => "service_name_invalid",
            StackError::PlatformInvalid(_) => "platform_invalid",
            StackError::JsonInvalid(_) => "json_invalid",
            StackError::FieldUnknown(_) => "field_unknown",
            StackError::FieldMissing(_) => "field_missing",
            StackError::FieldType(_) => "field_type",
            StackError::FieldToolOnly(_) => "field_tool_only",
            StackError::FormatUnsupported(_) => "format_unsupported",
            StackError::AppInvalid(_) => "app_invalid",
            StackError::AppMismatch { .. } => "app_mismatch",
            StackError::VersionInvalid(_) => "version_invalid",
            StackError::VersionMismatch { .. } => "version_mismatch",
            StackError::HealthTimeoutOutOfRange(_) => "health_timeout_out_of_range",
            StackError::RollbackInvalid(_) => "rollback_invalid",
            StackError::PreUpdateSameAsService => "pre_update_same_as_service",
            StackError::ComposeVersionInvalid(_) => "compose_version_invalid",
            StackError::PlatformsCount => "platforms_count",
            StackError::PlatformDuplicate(_) => "platform_duplicate",
            StackError::PublicKeysCount => "public_keys_count",
            StackError::PublicKeyInvalid(_) => "public_key_invalid",
            StackError::PublicKeyDuplicate(_) => "public_key_duplicate",
            StackError::ImagesCount => "images_count",
            StackError::AppControlInvalid(_) => "app_control_invalid",
            StackError::RequestInvalid(_) => "request_invalid",
            StackError::RequestTooLarge => "request_too_large",
            StackError::RequestFormat(_) => "request_format",
            StackError::RequestAction(_) => "request_action",
            StackError::RequestVersion(_) => "request_version",
            StackError::ArchiveInvalid(_) => "archive_invalid",
            StackError::ArchiveTooLarge => "archive_too_large",
            StackError::ArchiveTooManyEntries => "archive_too_many_entries",
            StackError::EntryType(_) => "entry_type",
            StackError::EntryPath(_) => "entry_path",
            StackError::EntryUnexpected(_) => "entry_unexpected",
            StackError::EntryDuplicate(_) => "entry_duplicate",
            StackError::EntryMissing(_) => "entry_missing",
            StackError::ComposeJsonInvalid(_) => "compose_json_invalid",
            StackError::ServiceMissing(_) => "service_missing",
            StackError::PreUpdateMissing(_) => "pre_update_missing",
            StackError::ImageMissing(_) => "image_missing",
            StackError::BuildPresent(_) => "build_present",
            StackError::ImageNotPinned { .. } => "image_not_pinned",
            StackError::ImageMalformed { .. } => "image_malformed",
            StackError::ImageOtherApp { .. } => "image_other_app",
            StackError::ImageNotDeclared { .. } => "image_not_declared",
            StackError::ImageUnused(_) => "image_unused",
            StackError::ProfileUnexpected(_) => "profile_unexpected",
            StackError::PreUpdateProfile(_) => "pre_update_profile",
            StackError::BindOutsideFiles(_) => "bind_outside_files",
            StackError::BindNotReadOnly(_) => "bind_not_read_only",
            StackError::BindPathInvalid(_) => "bind_path_invalid",
            StackError::ComposeFieldUnsupported(_) => "compose_field_unsupported",
            StackError::ComposeValueUnsupported { .. } => "compose_value_unsupported",
            StackError::ComposeTextHidden { .. } => "compose_text_hidden",
            StackError::EnvFileOutsideFiles(_) => "env_file_outside_files",
            StackError::ComposeNotJson(_) => "compose_not_json",
            StackError::EnvNameReserved(_) => "env_name_reserved",
            StackError::EnvLineUnsupported { .. } => "env_line_unsupported",
            StackError::OciJsonInvalid(_) => "oci_json_invalid",
            StackError::OciMediaType(_) => "oci_media_type",
            StackError::OciMediaTypeMismatch { .. } => "oci_media_type_mismatch",
            StackError::OciSchema => "oci_schema",
            StackError::OciLayerType(_) => "oci_layer_type",
            StackError::OciForeignLayer(_) => "oci_foreign_layer",
            StackError::OciPlatform(_) => "oci_platform",
            StackError::OciPlatformDuplicate(_) => "oci_platform_duplicate",
            StackError::OciChildType(_) => "oci_child_type",
        }
    }
}

/// Empreinte d'un objet du registre : `sha256:` suivi de 64 caractères hexadécimaux minuscules.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    pub const PREFIX: &'static str = "sha256:";

    pub fn parse(value: &str) -> Result<Self, StackError> {
        match value.strip_prefix(Self::PREFIX) {
            Some(hex)
                if hex.len() == 64
                    && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Ok(Self(value.to_owned()))
            }
            _ => Err(StackError::DigestInvalid(value.to_owned())),
        }
    }

    /// Empreinte de données en mémoire.
    pub fn of(data: &[u8]) -> Self {
        let hash = Sha256::digest(data);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hash);
        Self::from_sha256(bytes)
    }

    /// Empreinte d'un SHA-256 déjà calculé, par exemple en flux.
    pub fn from_sha256(bytes: [u8; 32]) -> Self {
        let mut text = String::with_capacity(71);
        text.push_str(Self::PREFIX);
        for byte in bytes {
            text.push_str(&format!("{byte:02x}"));
        }
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Les 64 caractères hexadécimaux, sans le préfixe : le nom du fichier sur le disque.
    pub fn hex(&self) -> &str {
        &self.0[Self::PREFIX.len()..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Digest {
    type Err = StackError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Nom d'image dans le registre, après `<app>/` : `[a-z0-9]+([._-][a-z0-9]+)*`, 64 caractères au plus.
pub fn is_image_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut previous_separator = true;
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_separator = false,
            b'.' | b'_' | b'-' if !previous_separator => previous_separator = true,
            _ => return false,
        }
    }
    !previous_separator
}

/// Nom de service Compose : `[A-Za-z0-9._-]{1,64}`.
pub fn is_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Plateforme Docker d'une arch DrakBase (table de docs/PROTOCOLE.md), toujours sous Linux.
/// `universal` n'en a pas.
pub const fn docker_platform(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::X86_64 => Some("linux/amd64"),
        Arch::Aarch64 => Some("linux/arm64"),
        Arch::Armv7 => Some("linux/arm/v7"),
        Arch::Riscv64 => Some("linux/riscv64"),
        Arch::I686 => Some("linux/386"),
        Arch::Universal => None,
    }
}

/// Arch DrakBase d'une plateforme Docker donnée par ses parties (`os`, `architecture`, `variant`
/// d'une configuration ou d'un index). `None` si elle n'est pas dans la table.
pub fn arch_of_platform(os: &str, architecture: &str, variant: Option<&str>) -> Option<Arch> {
    if os != "linux" {
        return None;
    }
    match (architecture, variant.filter(|v| !v.is_empty())) {
        ("amd64", None) => Some(Arch::X86_64),
        ("arm64", None | Some("v8")) => Some(Arch::Aarch64),
        ("arm", Some("v7")) => Some(Arch::Armv7),
        ("riscv64", None) => Some(Arch::Riscv64),
        ("386", None) => Some(Arch::I686),
        _ => None,
    }
}

/// Arch DrakBase d'une plateforme écrite `os/architecture[/variant]`, comme dans `stack.json`.
pub fn parse_docker_platform(value: &str) -> Result<Arch, StackError> {
    let mut parts = value.split('/');
    let os = parts.next().unwrap_or_default();
    let architecture = parts.next().unwrap_or_default();
    let variant = parts.next();
    if parts.next().is_some() {
        return Err(StackError::PlatformInvalid(value.to_owned()));
    }
    arch_of_platform(os, architecture, variant)
        .ok_or_else(|| StackError::PlatformInvalid(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests() {
        let digest = Digest::of(b"abc");
        assert_eq!(
            digest.as_str(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Digest::parse(digest.as_str()).unwrap(), digest);
        assert_eq!(digest.hex().len(), 64);
        for bad in [
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "sha512:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:ba7816bf",
            "latest",
            "",
        ] {
            assert_eq!(
                Digest::parse(bad).unwrap_err().code(),
                "digest_invalid",
                "{bad}"
            );
        }
    }

    #[test]
    fn names() {
        for good in [
            "home-manager",
            "a",
            "web.ui",
            "db_2",
            "x".repeat(64).as_str(),
        ] {
            assert!(is_image_name(good), "{good}");
        }
        for bad in [
            "",
            "Home",
            "-a",
            "a-",
            "a--b",
            "a/b",
            "x".repeat(65).as_str(),
        ] {
            assert!(!is_image_name(bad), "{bad}");
        }
        assert!(is_service_name("Home_Manager-2.web"));
        assert!(!is_service_name("a b"));
        assert!(!is_service_name(""));
        assert!(is_app_slug("home-manager"));
        assert!(!is_app_slug("Home"));
    }

    #[test]
    fn platforms() {
        for arch in [
            Arch::X86_64,
            Arch::Aarch64,
            Arch::Armv7,
            Arch::Riscv64,
            Arch::I686,
        ] {
            let name = docker_platform(arch).unwrap();
            assert_eq!(parse_docker_platform(name).unwrap(), arch, "{name}");
        }
        assert_eq!(
            parse_docker_platform("linux/arm64/v8").unwrap(),
            Arch::Aarch64
        );
        assert_eq!(docker_platform(Arch::Universal), None);
        for bad in [
            "linux/arm",
            "linux/arm/v6",
            "windows/amd64",
            "unknown/unknown",
            "linux/arm64/v8/x",
            "amd64",
        ] {
            assert_eq!(
                parse_docker_platform(bad).unwrap_err().code(),
                "platform_invalid",
                "{bad}"
            );
        }
    }
}
