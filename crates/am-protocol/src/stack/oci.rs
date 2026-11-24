//! Manifestes, index et configurations d'images OCI ou Docker, tels que l'outil les envoie et
//! que le registre les sert (docs/API.md, « Stacks Docker », publication des images).

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::{Digest, StackError, arch_of_platform, docker_platform};
use crate::platform::Arch;

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const MANIFEST_TYPES: [&str; 4] = [
    OCI_INDEX,
    OCI_MANIFEST,
    DOCKER_MANIFEST_LIST,
    DOCKER_MANIFEST,
];

pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";

/// Couche sans compression.
pub const TAR_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
/// Couche compressée en gzip.
pub const GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// Couche compressée en zstd : le type des couches que l'outil publie.
pub const ZSTD_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
/// Couche en gzip, sous son nom Docker.
pub const DOCKER_GZIP_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
/// Les types de couches admis.
pub const LAYER_TYPES: [&str; 4] = [TAR_LAYER, GZIP_LAYER, ZSTD_LAYER, DOCKER_GZIP_LAYER];
const FOREIGN_LAYER_DOCKER: &str = "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip";
const NONDISTRIBUTABLE_PREFIX: &str = "application/vnd.oci.image.layer.nondistributable.";

/// Taille maximale d'un manifeste envoyé au serveur.
pub const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

/// Ce que désigne un objet du registre.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefKind {
    /// Configuration ou couche.
    Blob,
    /// Manifeste d'image désigné par un index.
    Manifest,
}

impl RefKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RefKind::Blob => "blob",
            RefKind::Manifest => "manifest",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub descriptor: Descriptor,
    pub arch: Arch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OciObject {
    /// Index (ou liste de manifestes Docker) : un manifeste d'image par plateforme.
    Index(Vec<IndexEntry>),
    /// Manifeste d'image : configuration et couches.
    Image {
        config: Descriptor,
        layers: Vec<Descriptor>,
    },
}

impl OciObject {
    /// Objets désignés, dans l'ordre du manifeste.
    pub fn references(&self) -> Vec<(Digest, RefKind)> {
        match self {
            OciObject::Index(entries) => entries
                .iter()
                .map(|entry| (entry.descriptor.digest.clone(), RefKind::Manifest))
                .collect(),
            OciObject::Image { config, layers } => std::iter::once(config)
                .chain(layers)
                .map(|descriptor| (descriptor.digest.clone(), RefKind::Blob))
                .collect(),
        }
    }

    /// Plateformes d'un index, au format Docker (`linux/arm64`) ; vide pour un manifeste d'image.
    pub fn platforms(&self) -> Vec<&'static str> {
        match self {
            OciObject::Index(entries) => entries
                .iter()
                .filter_map(|entry| docker_platform(entry.arch))
                .collect(),
            OciObject::Image { .. } => Vec::new(),
        }
    }
}

fn invalid(what: impl Into<String>) -> StackError {
    StackError::OciJsonInvalid(what.into())
}

fn descriptor(value: &Value) -> Result<Descriptor, StackError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("descripteur : objet attendu"))?;
    let media_type = map
        .get("mediaType")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("descripteur sans mediaType"))?;
    let digest = map
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("descripteur sans digest"))?;
    let size = map
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("descripteur sans size"))?;
    Ok(Descriptor {
        media_type: media_type.to_owned(),
        digest: Digest::parse(digest)?,
        size,
    })
}

fn platform_text(map: &Map<String, Value>) -> String {
    let field = |name: &str| map.get(name).and_then(Value::as_str).unwrap_or_default();
    let mut text = format!("{}/{}", field("os"), field("architecture"));
    if !field("variant").is_empty() {
        text.push('/');
        text.push_str(field("variant"));
    }
    text
}

fn platform_arch(map: &Map<String, Value>) -> Result<Arch, StackError> {
    let field = |name: &str| map.get(name).and_then(Value::as_str);
    arch_of_platform(
        field("os").unwrap_or_default(),
        field("architecture").unwrap_or_default(),
        field("variant"),
    )
    .ok_or_else(|| StackError::OciPlatform(platform_text(map)))
}

/// Type d'un manifeste, sans les paramètres d'un `Content-Type` (`; charset=…`).
fn bare_media_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or_default().trim()
}

/// Lit un manifeste ou un index envoyé avec ce `Content-Type`, et contrôle ses règles.
pub fn parse_manifest(body: &[u8], content_type: &str) -> Result<OciObject, StackError> {
    let declared = bare_media_type(content_type);
    if !MANIFEST_TYPES.contains(&declared) {
        return Err(StackError::OciMediaType(declared.to_owned()));
    }
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(invalid("manifeste de plus de 2 Mio"));
    }
    let root: Value = serde_json::from_slice(body).map_err(|error| invalid(error.to_string()))?;
    let root = root.as_object().ok_or_else(|| invalid("objet attendu"))?;
    if root.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        return Err(StackError::OciSchema);
    }
    if let Some(found) = root.get("mediaType").and_then(Value::as_str)
        && found != declared
    {
        return Err(StackError::OciMediaTypeMismatch {
            declared: declared.to_owned(),
            found: found.to_owned(),
        });
    }

    if declared == OCI_INDEX || declared == DOCKER_MANIFEST_LIST {
        let manifests = root
            .get("manifests")
            .and_then(Value::as_array)
            .filter(|manifests| !manifests.is_empty())
            .ok_or_else(|| invalid("index sans manifeste"))?;
        let mut seen = BTreeSet::new();
        let mut entries = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            let descriptor = descriptor(manifest)?;
            if descriptor.media_type != OCI_MANIFEST && descriptor.media_type != DOCKER_MANIFEST {
                return Err(StackError::OciChildType(descriptor.media_type));
            }
            let platform = manifest
                .get("platform")
                .and_then(Value::as_object)
                .ok_or_else(|| StackError::OciPlatform("absente".into()))?;
            let arch = platform_arch(platform)?;
            if !seen.insert(arch) {
                return Err(StackError::OciPlatformDuplicate(platform_text(platform)));
            }
            entries.push(IndexEntry { descriptor, arch });
        }
        return Ok(OciObject::Index(entries));
    }

    let config = descriptor(
        root.get("config")
            .ok_or_else(|| invalid("manifeste sans config"))?,
    )?;
    if config.media_type != OCI_CONFIG && config.media_type != DOCKER_CONFIG {
        return Err(StackError::OciMediaType(config.media_type));
    }
    let mut layers = Vec::new();
    for layer in root
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("manifeste sans layers"))?
    {
        let descriptor = descriptor(layer)?;
        let has_urls = layer
            .get("urls")
            .and_then(Value::as_array)
            .is_some_and(|urls| !urls.is_empty());
        if has_urls
            || descriptor.media_type == FOREIGN_LAYER_DOCKER
            || descriptor.media_type.starts_with(NONDISTRIBUTABLE_PREFIX)
        {
            return Err(StackError::OciForeignLayer(descriptor.digest.to_string()));
        }
        if !LAYER_TYPES.contains(&descriptor.media_type.as_str()) {
            return Err(StackError::OciLayerType(descriptor.media_type));
        }
        layers.push(descriptor);
    }
    Ok(OciObject::Image { config, layers })
}

/// Arch d'une configuration d'image (`os`, `architecture`, `variant`).
pub fn config_arch(config: &[u8]) -> Result<Arch, StackError> {
    let root: Value = serde_json::from_slice(config).map_err(|error| invalid(error.to_string()))?;
    platform_arch(
        root.as_object()
            .ok_or_else(|| invalid("configuration : objet attendu"))?,
    )
}
