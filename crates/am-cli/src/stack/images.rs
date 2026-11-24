//! Images d'une stack : lecture des dossiers OCI qu'écrit `docker buildx build`, recompression
//! des couches en zstd niveau 19, réécriture des manifestes et de l'index, puis envoi au serveur
//! des seuls objets qui lui manquent (docs/API.md, « Stacks Docker », publication des images).
//!
//! L'outil recompresse lui-même : le cache de BuildKit rendrait sinon une couche déjà compressée
//! à un autre niveau (essai S0). La compression se fait sur un seul fil par couche, ce qui la rend
//! reproductible : les mêmes octets donnent la même couche, donc une couche qui ne change pas
//! garde son empreinte, et le serveur l'a déjà.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use am_protocol::Arch;
use am_protocol::stack::oci::{
    self, DOCKER_GZIP_LAYER, DOCKER_MANIFEST, DOCKER_MANIFEST_LIST, GZIP_LAYER, OCI_INDEX,
    OCI_MANIFEST, TAR_LAYER, ZSTD_LAYER,
};
use am_protocol::stack::{Digest, arch_of_platform, docker_platform};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::api::Api;
use crate::connection::failure;
use crate::home::AppSlug;

/// Niveau de compression des couches publiées.
pub const ZSTD_LEVEL: i32 = 19;
/// Couches recompressées en même temps : chacune prend une centaine de Mo de mémoire au niveau 19.
const MAX_WORKERS: usize = 4;

/// Une couche ou une configuration, dans un fichier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub digest: Digest,
    pub size: u64,
    pub path: PathBuf,
}

/// Un manifeste d'image ou un index, en JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub digest: Digest,
    pub media_type: &'static str,
    pub body: Vec<u8>,
}

impl Document {
    fn new(media_type: &'static str, value: &Value) -> Result<Self> {
        let body = serde_json::to_vec(value)?;
        // Les mêmes règles que le serveur, avant tout envoi.
        oci::parse_manifest(&body, media_type)
            .with_context(|| format!("manifeste réécrit refusé ({media_type})"))?;
        Ok(Self {
            digest: Digest::of(&body),
            media_type,
            body,
        })
    }
}

/// L'image d'une plateforme : son manifeste, sa configuration et ses couches.
#[derive(Debug)]
pub struct PlatformImage {
    pub arch: Arch,
    pub manifest: Document,
    pub config: Blob,
    pub layers: Vec<Blob>,
}

impl PlatformImage {
    /// Taille de la configuration et des couches : ce que télécharge une première installation.
    pub fn size(&self) -> u64 {
        self.config.size + self.layers.iter().map(|layer| layer.size).sum::<u64>()
    }
}

/// Une image prête à envoyer : une plateforme par arch de `stack.json`, et l'index qui les réunit.
#[derive(Debug)]
pub struct PreparedImage {
    pub name: String,
    pub platforms: Vec<PlatformImage>,
    pub index: Document,
}

/// Un dossier au format OCI (`oci-layout`, `index.json`, `blobs/sha256/…`).
struct Layout {
    dir: PathBuf,
}

impl Layout {
    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.dir.join("blobs/sha256").join(digest.hex())
    }

    /// Un JSON du dossier, contrôlé par son empreinte.
    fn read_json(&self, digest: &Digest) -> Result<(Vec<u8>, Value)> {
        let path = self.blob_path(digest);
        let bytes = fs::read(&path).with_context(|| format!("lecture de {}", path.display()))?;
        ensure!(
            Digest::of(&bytes) == *digest,
            "{} ne correspond pas à son empreinte",
            path.display()
        );
        let value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} : JSON illisible", path.display()))?;
        Ok((bytes, value))
    }
}

/// Un manifeste d'image du dossier OCI, avec sa plateforme.
struct Found {
    arch: Arch,
    /// `platform` de l'index : `{architecture, os, variant?}`.
    platform: Value,
    digest: Digest,
}

fn descriptor_digest(descriptor: &Value) -> Result<Digest> {
    Digest::parse(
        descriptor
            .get("digest")
            .and_then(Value::as_str)
            .context("descripteur sans empreinte")?,
    )
    .map_err(Into::into)
}

fn platform_of(value: &Map<String, Value>) -> Option<(Arch, Value)> {
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    let arch = arch_of_platform(
        field("os").unwrap_or_default(),
        field("architecture").unwrap_or_default(),
        field("variant"),
    )?;
    let mut platform = json!({
        "architecture": field("architecture").unwrap_or_default(),
        "os": field("os").unwrap_or_default(),
    });
    if let Some(variant) = field("variant").filter(|variant| !variant.is_empty()) {
        platform["variant"] = json!(variant);
    }
    Some((arch, platform))
}

/// Les manifestes d'image d'un dossier OCI : ceux d'un index (une construction multi-plateforme),
/// ou un manifeste seul, dont la plateforme se lit dans sa configuration. Les manifestes sans
/// plateforme de la table (attestations) sont laissés de côté.
fn image_manifests(layout: &Layout) -> Result<Vec<Found>> {
    let index_path = layout.dir.join("index.json");
    let top: Value = serde_json::from_slice(
        &fs::read(&index_path).with_context(|| format!("lecture de {}", index_path.display()))?,
    )
    .with_context(|| format!("{} illisible", index_path.display()))?;
    let mut found: Vec<Found> = Vec::new();
    for descriptor in top
        .get("manifests")
        .and_then(Value::as_array)
        .with_context(|| format!("{} sans manifestes", index_path.display()))?
    {
        let digest = descriptor_digest(descriptor)?;
        let media_type = descriptor
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match media_type {
            OCI_INDEX | DOCKER_MANIFEST_LIST => {
                let (_, index) = layout.read_json(&digest)?;
                for child in index
                    .get("manifests")
                    .and_then(Value::as_array)
                    .context("index sans manifestes")?
                {
                    let Some((arch, platform)) = child
                        .get("platform")
                        .and_then(Value::as_object)
                        .and_then(platform_of)
                    else {
                        continue;
                    };
                    found.push(Found {
                        arch,
                        platform,
                        digest: descriptor_digest(child)?,
                    });
                }
            }
            OCI_MANIFEST | DOCKER_MANIFEST => {
                let (_, manifest) = layout.read_json(&digest)?;
                let config = descriptor_digest(
                    manifest
                        .get("config")
                        .context("manifeste sans configuration")?,
                )?;
                let (_, config) = layout.read_json(&config)?;
                if let Some((arch, platform)) = config.as_object().and_then(platform_of) {
                    found.push(Found {
                        arch,
                        platform,
                        digest,
                    });
                }
            }
            other => bail!("{} : type « {other} » inattendu", index_path.display()),
        }
    }
    Ok(found)
}

/// Une couche à recompresser.
struct Job {
    source: PathBuf,
    media_type: String,
    /// `rootfs.diff_ids` de la configuration : le SHA-256 de la couche décompressée.
    diff_id: Digest,
}

/// Lit le dossier OCI d'une image construite pour ces plateformes, recompresse ses couches dans
/// `work` (au niveau [`ZSTD_LEVEL`]) et réécrit ses manifestes et son index.
pub fn prepare(
    name: &str,
    oci_dir: &Path,
    platforms: &[Arch],
    work: &Path,
) -> Result<PreparedImage> {
    let layout = Layout {
        dir: oci_dir.to_owned(),
    };
    let found = image_manifests(&layout)
        .with_context(|| format!("image {name} : dossier OCI {}", oci_dir.display()))?;

    // Manifestes, configurations et couches à recompresser, une plateforme après l'autre.
    struct Source {
        arch: Arch,
        platform: Value,
        manifest: Value,
        config: Blob,
        config_type: String,
        layers: Vec<Digest>,
    }
    let mut sources = Vec::new();
    let mut jobs: BTreeMap<Digest, Job> = BTreeMap::new();
    for arch in platforms {
        let platform_name = docker_platform(*arch).unwrap_or("?");
        let mut matching = found.iter().filter(|candidate| candidate.arch == *arch);
        let entry = matching.next().with_context(|| {
            format!("image {name} : buildx n'a pas produit la plateforme {platform_name}")
        })?;
        ensure!(
            matching.next().is_none(),
            "image {name} : plusieurs manifestes pour la plateforme {platform_name}"
        );
        let (_, manifest) = layout.read_json(&entry.digest)?;
        let config_descriptor = manifest
            .get("config")
            .context("manifeste sans configuration")?;
        let config_digest = descriptor_digest(config_descriptor)?;
        let (config_bytes, config) = layout.read_json(&config_digest)?;
        let diff_ids: Vec<Digest> = config
            .pointer("/rootfs/diff_ids")
            .and_then(Value::as_array)
            .context("configuration sans rootfs.diff_ids")?
            .iter()
            .map(|id| Digest::parse(id.as_str().unwrap_or_default()))
            .collect::<Result<_, _>>()?;
        let layers = manifest
            .get("layers")
            .and_then(Value::as_array)
            .context("manifeste sans couches")?;
        ensure!(
            layers.len() == diff_ids.len(),
            "image {name} ({platform_name}) : {} couches pour {} diff_ids",
            layers.len(),
            diff_ids.len()
        );
        let mut layer_digests = Vec::new();
        for (layer, diff_id) in layers.iter().zip(diff_ids) {
            let digest = descriptor_digest(layer)?;
            let media_type = layer
                .get("mediaType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            jobs.entry(digest.clone()).or_insert_with(|| Job {
                source: layout.blob_path(&digest),
                media_type,
                diff_id,
            });
            layer_digests.push(digest);
        }
        sources.push(Source {
            arch: *arch,
            platform: entry.platform.clone(),
            config_type: config_descriptor
                .get("mediaType")
                .and_then(Value::as_str)
                .unwrap_or(oci::OCI_CONFIG)
                .to_owned(),
            config: Blob {
                digest: config_digest.clone(),
                size: config_bytes.len() as u64,
                path: layout.blob_path(&config_digest),
            },
            manifest,
            layers: layer_digests,
        });
    }

    let recompressed = recompress_all(&jobs, work)?;
    let mut prepared = Vec::new();
    let mut entries = Vec::new();
    for source in sources {
        let layers: Vec<Blob> = source
            .layers
            .iter()
            .map(|digest| recompressed[digest].clone())
            .collect();
        let mut manifest = json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": {
                "mediaType": source.config_type,
                "digest": source.config.digest,
                "size": source.config.size,
            },
            "layers": layers
                .iter()
                .map(|layer| json!({"mediaType": ZSTD_LAYER, "digest": layer.digest, "size": layer.size}))
                .collect::<Vec<_>>(),
        });
        if let Some(annotations) = source.manifest.get("annotations") {
            manifest["annotations"] = annotations.clone();
        }
        let manifest = Document::new(OCI_MANIFEST, &manifest)?;
        entries.push(json!({
            "mediaType": OCI_MANIFEST,
            "digest": manifest.digest,
            "size": manifest.body.len(),
            "platform": source.platform,
        }));
        prepared.push(PlatformImage {
            arch: source.arch,
            manifest,
            config: source.config,
            layers,
        });
    }
    let index = Document::new(
        OCI_INDEX,
        &json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": entries}),
    )?;
    Ok(PreparedImage {
        name: name.to_owned(),
        platforms: prepared,
        index,
    })
}

/// Recompresse les couches, plusieurs à la fois ; rend la couche zstd de chaque couche d'origine.
fn recompress_all(jobs: &BTreeMap<Digest, Job>, work: &Path) -> Result<BTreeMap<Digest, Blob>> {
    fs::create_dir_all(work).with_context(|| format!("création de {}", work.display()))?;
    let jobs: Vec<(&Digest, &Job)> = jobs.iter().collect();
    let next = AtomicUsize::new(0);
    let results: Mutex<BTreeMap<Digest, Result<Blob>>> = Mutex::new(BTreeMap::new());
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .clamp(1, MAX_WORKERS)
        .min(jobs.len().max(1));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some((digest, job)) = jobs.get(index) else {
                        break;
                    };
                    let result = recompress(job, work);
                    if let Ok(blob) = &result {
                        println!(
                            "  couche {} : {} → {} en zstd -{ZSTD_LEVEL}",
                            short(digest),
                            am_protocol::human_size(
                                fs::metadata(&job.source).map_or(0, |m| m.len())
                            ),
                            am_protocol::human_size(blob.size)
                        );
                    }
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert((*digest).clone(), result);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .into_iter()
        .map(|(digest, result)| {
            result
                .with_context(|| format!("recompression de la couche {digest}"))
                .map(|blob| (digest, blob))
        })
        .collect()
}

/// Les 12 premiers caractères hexadécimaux, pour les messages.
pub fn short(digest: &Digest) -> String {
    format!("{}…", &digest.hex()[..12])
}

/// Écrit dans un fichier en calculant le SHA-256 et la taille de ce qui passe.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    size: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn digest_of(hasher: Sha256) -> Digest {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&hasher.finalize());
    Digest::from_sha256(bytes)
}

/// Décompresse une couche, contrôle son `diff_id`, et la recompresse en zstd dans `work`.
fn recompress(job: &Job, work: &Path) -> Result<Blob> {
    let file = BufReader::new(
        File::open(&job.source).with_context(|| format!("lecture de {}", job.source.display()))?,
    );
    let mut reader: Box<dyn Read> = match job.media_type.as_str() {
        GZIP_LAYER | DOCKER_GZIP_LAYER => Box::new(flate2::read::MultiGzDecoder::new(file)),
        ZSTD_LAYER => Box::new(zstd::stream::read::Decoder::new(file)?),
        TAR_LAYER => Box::new(file),
        other => bail!("type de couche « {other} » non pris en charge"),
    };
    let temp = tempfile::NamedTempFile::new_in(work)?;
    let mut encoder = zstd::stream::write::Encoder::new(
        Hashing {
            inner: BufWriter::new(temp.as_file().try_clone()?),
            hasher: Sha256::new(),
            size: 0,
        },
        ZSTD_LEVEL,
    )?;
    encoder.include_checksum(true)?;
    let mut uncompressed = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        uncompressed.update(&buffer[..read]);
        encoder.write_all(&buffer[..read])?;
    }
    let mut hashing = encoder.finish()?;
    hashing.flush()?;
    let diff_id = digest_of(uncompressed);
    ensure!(
        diff_id == job.diff_id,
        "son contenu ({diff_id}) n'est pas celui qu'annonce la configuration ({})",
        job.diff_id
    );
    let digest = digest_of(hashing.hasher);
    let path = work.join(digest.hex());
    temp.persist(&path)
        .map_err(|error| anyhow!("écriture de {} : {}", path.display(), error.error))?;
    Ok(Blob {
        digest,
        size: hashing.size,
        path,
    })
}

/// Envoie les images : d'abord les couches et configurations qui manquent au serveur (`HEAD` puis
/// `PUT`), puis les manifestes d'image, puis les index. `check_size` refuse tôt un objet plus
/// gros que ce qu'accepte le serveur.
pub fn upload(
    api: &Api,
    app: &AppSlug,
    images: &[PreparedImage],
    check_size: &dyn Fn(&Path, u64) -> Result<()>,
) -> Result<()> {
    let mut seen: BTreeSet<&Digest> = BTreeSet::new();
    for image in images {
        let mut sent = (0usize, 0u64);
        let mut kept = 0usize;
        for platform in &image.platforms {
            for blob in std::iter::once(&platform.config).chain(&platform.layers) {
                if !seen.insert(&blob.digest) {
                    continue;
                }
                let present = api
                    .blob_size(app, &blob.digest)
                    .map_err(|error| failure(error, format!("couche {}", blob.digest)))?;
                if present.is_some() {
                    kept += 1;
                    continue;
                }
                check_size(&blob.path, blob.size)?;
                api.put_blob(
                    app,
                    &blob.digest,
                    &blob.path,
                    blob.size,
                    &format!("{} {}", image.name, short(&blob.digest)),
                )
                .map_err(|error| failure(error, format!("envoi de la couche {}", blob.digest)))?;
                sent.0 += 1;
                sent.1 += blob.size;
            }
        }
        for document in image
            .platforms
            .iter()
            .map(|platform| &platform.manifest)
            .chain(std::iter::once(&image.index))
        {
            let present = api
                .has_manifest(app, &document.digest)
                .map_err(|error| failure(error, format!("manifeste {}", document.digest)))?;
            if present {
                kept += 1;
                continue;
            }
            api.put_manifest(app, &document.digest, document.media_type, &document.body)
                .map_err(|error| {
                    failure(error, format!("envoi du manifeste {}", document.digest))
                })?;
            sent.0 += 1;
            sent.1 += document.body.len() as u64;
        }
        let platforms: Vec<String> = image
            .platforms
            .iter()
            .map(|platform| {
                format!(
                    "{} {}",
                    platform.arch,
                    am_protocol::human_size(platform.size())
                )
            })
            .collect();
        let what = if sent.0 == 0 {
            "tout était déjà sur le serveur".to_owned()
        } else {
            format!(
                "{} objet(s) envoyé(s) ({}), {kept} déjà sur le serveur",
                sent.0,
                am_protocol::human_size(sent.1)
            )
        };
        println!(
            "✓ image {} : {} ({}), {what}",
            image.name,
            image.index.digest,
            platforms.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use am_protocol::stack::oci::OciObject;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// Un document réécrit, relu comme le relira le serveur.
    fn reread(document: &Document) -> OciObject {
        oci::parse_manifest(&document.body, document.media_type).unwrap()
    }

    /// Une couche : un tar d'un fichier.
    fn layer_tar(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        builder.append_data(&mut header, name, contents).unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn put(dir: &Path, data: &[u8]) -> Digest {
        let digest = Digest::of(data);
        fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        fs::write(dir.join("blobs/sha256").join(digest.hex()), data).unwrap();
        digest
    }

    /// Un dossier OCI comme celui de `docker buildx build --output type=oci,tar=false` : un index
    /// imbriqué, une image par arch, chacune de deux couches en gzip, la première commune.
    fn oci_layout(dir: &Path, archs: &[(&str, &str)], marker: &str) {
        let base = layer_tar("base.txt", b"socle commun");
        let base_gz = gzip(&base);
        let mut entries = Vec::new();
        for (architecture, variant) in archs {
            let own = layer_tar("app.txt", format!("{marker} {architecture}").as_bytes());
            let own_gz = gzip(&own);
            let config = serde_json::to_vec(&json!({
                "architecture": architecture,
                "os": "linux",
                "variant": variant,
                "rootfs": {"type": "layers", "diff_ids": [Digest::of(&base), Digest::of(&own)]},
            }))
            .unwrap();
            let config_digest = put(dir, &config);
            let layers: Vec<Value> = [&base_gz, &own_gz]
                .iter()
                .map(|layer| {
                    json!({
                        "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": put(dir, layer),
                        "size": layer.len(),
                    })
                })
                .collect();
            let manifest = serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": OCI_MANIFEST,
                "config": {"mediaType": oci::OCI_CONFIG, "digest": config_digest, "size": config.len()},
                "layers": layers,
            }))
            .unwrap();
            let mut platform = json!({"architecture": architecture, "os": "linux"});
            if !variant.is_empty() {
                platform["variant"] = json!(variant);
            }
            entries.push(json!({
                "mediaType": OCI_MANIFEST,
                "digest": put(dir, &manifest),
                "size": manifest.len(),
                "platform": platform,
            }));
        }
        // Un manifeste d'attestation, laissé de côté.
        entries.push(json!({
            "mediaType": OCI_MANIFEST,
            "digest": put(dir, b"{}"),
            "size": 2,
            "platform": {"architecture": "unknown", "os": "unknown"},
        }));
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": entries,
        }))
        .unwrap();
        let index_digest = put(dir, &index);
        fs::write(
            dir.join("index.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": OCI_INDEX,
                "manifests": [{"mediaType": OCI_INDEX, "digest": index_digest, "size": index.len()}],
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    }

    /// Les constantes de ce module désignent les types de couches par leur rang dans le contrat.
    #[test]
    fn layers_are_recompressed_and_manifests_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let oci_dir = dir.path().join("oci");
        oci_layout(&oci_dir, &[("arm64", "v8"), ("amd64", "")], "v1");
        let platforms = [Arch::X86_64, Arch::Aarch64];
        let image = prepare("web", &oci_dir, &platforms, &dir.path().join("work")).unwrap();

        // L'index suit l'ordre de stack.json, et se relit comme le relira le serveur.
        let OciObject::Index(entries) = reread(&image.index) else {
            panic!("index attendu");
        };
        let archs: Vec<Arch> = entries.iter().map(|entry| entry.arch).collect();
        assert_eq!(archs, platforms);
        assert_eq!(
            entries[1].descriptor.digest,
            image.platforms[1].manifest.digest
        );
        let index: Value = serde_json::from_slice(&image.index.body).unwrap();
        assert_eq!(
            index["manifests"][1]["platform"],
            json!({"architecture": "arm64", "os": "linux", "variant": "v8"})
        );

        for platform in &image.platforms {
            let OciObject::Image { config, layers } = reread(&platform.manifest) else {
                panic!("manifeste d'image attendu");
            };
            assert_eq!(config.digest, platform.config.digest);
            assert_eq!(layers.len(), 2);
            for (descriptor, blob) in layers.iter().zip(&platform.layers) {
                assert_eq!(descriptor.media_type, ZSTD_LAYER);
                assert_eq!(descriptor.digest, blob.digest);
                let bytes = fs::read(&blob.path).unwrap();
                assert_eq!(bytes.len() as u64, blob.size);
                assert_eq!(Digest::of(&bytes), blob.digest);
                // Une vraie trame zstd, qui rend la couche d'origine.
                assert_eq!(&bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
            }
        }
        // La couche commune n'est recompressée qu'une fois, et garde son empreinte.
        assert_eq!(image.platforms[0].layers[0], image.platforms[1].layers[0]);
        let decoded = zstd::decode_all(
            fs::read(&image.platforms[0].layers[1].path)
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(decoded, layer_tar("app.txt", b"v1 amd64"));

        // Les mêmes couches donnent les mêmes octets, même construites une autre fois.
        let again = prepare("web", &oci_dir, &platforms, &dir.path().join("work2")).unwrap();
        assert_eq!(again.index.digest, image.index.digest);
        let other_dir = dir.path().join("oci2");
        oci_layout(&other_dir, &[("arm64", "v8"), ("amd64", "")], "v2");
        let next = prepare("web", &other_dir, &platforms, &dir.path().join("work3")).unwrap();
        assert_eq!(
            next.platforms[0].layers[0].digest,
            image.platforms[0].layers[0].digest
        );
        assert_ne!(
            next.platforms[0].layers[1].digest,
            image.platforms[0].layers[1].digest
        );
        assert_ne!(next.index.digest, image.index.digest);
    }

    #[test]
    fn a_missing_platform_or_a_damaged_layer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let oci_dir = dir.path().join("oci");
        oci_layout(&oci_dir, &[("arm64", "")], "v1");
        let error = format!(
            "{:#}",
            prepare(
                "web",
                &oci_dir,
                &[Arch::Aarch64, Arch::X86_64],
                &dir.path().join("w")
            )
            .unwrap_err()
        );
        assert!(
            error.contains("buildx n'a pas produit la plateforme linux/amd64"),
            "{error}"
        );

        // Une couche dont le contenu n'est pas celui qu'annonce la configuration.
        let layout = Layout {
            dir: oci_dir.clone(),
        };
        let manifest_entry = image_manifests(&layout).unwrap().remove(0);
        let (_, manifest) = layout.read_json(&manifest_entry.digest).unwrap();
        let layer = descriptor_digest(&manifest["layers"][1]).unwrap();
        fs::write(layout.blob_path(&layer), gzip(b"autre chose")).unwrap();
        let error = format!(
            "{:#}",
            prepare("web", &oci_dir, &[Arch::Aarch64], &dir.path().join("w3")).unwrap_err()
        );
        assert!(
            error.contains("n'est pas celui qu'annonce la configuration"),
            "{error}"
        );
    }

    fn stack_server() -> (crate::fake_server::FakeServer, Api, AppSlug) {
        use crate::credentials::{ServerUrl, Token};
        let mut state = crate::fake_server::State::demo(None);
        let app = state.apps.get_mut("test-app").unwrap();
        app.kind = "stack".to_owned();
        let server = crate::fake_server::FakeServer::start(state);
        let api = Api::new(
            ServerUrl::parse(&server.url).unwrap(),
            Token::parse("amk_essai_complet").unwrap(),
        )
        .unwrap();
        (server, api, "test-app".parse().unwrap())
    }

    #[test]
    fn only_what_the_server_lacks_is_sent_blobs_first() {
        let dir = tempfile::tempdir().unwrap();
        let oci_dir = dir.path().join("oci");
        oci_layout(&oci_dir, &[("arm64", ""), ("amd64", "")], "v1");
        let platforms = [Arch::Aarch64, Arch::X86_64];
        let image = prepare("web", &oci_dir, &platforms, &dir.path().join("w")).unwrap();
        let (server, api, app) = stack_server();
        let no_limit = |_: &Path, _: u64| Ok(());

        // Une coupure au milieu de la première couche : la même commande reprend.
        server.state().cut_upload = Some(("blob".to_owned(), 1));
        let error = format!(
            "{:#}",
            upload(&api, &app, std::slice::from_ref(&image), &no_limit).unwrap_err()
        );
        assert!(error.contains("envoi de la couche"), "{error}");
        upload(&api, &app, std::slice::from_ref(&image), &no_limit).unwrap();
        {
            let state = server.state();
            // 2 configurations, 3 couches (dont une commune), 2 manifestes, 1 index.
            assert_eq!(state.blobs["test-app"].len(), 5);
            assert_eq!(state.manifests["test-app"].len(), 3);
            let index = &state.manifests["test-app"][image.index.digest.as_str()];
            assert_eq!(index.archs, platforms);
            let requests = state.image_requests();
            let last_blob = requests.iter().rposition(|r| r == "PUT blobs").unwrap();
            let first_manifest = requests.iter().position(|r| r == "PUT manifests").unwrap();
            assert!(last_blob < first_manifest, "{requests:?}");
            // HEAD avant chaque PUT.
            assert_eq!(requests.iter().filter(|r| *r == "PUT manifests").count(), 3);
            assert_eq!(
                requests.iter().filter(|r| *r == "HEAD manifests").count(),
                3
            );
        }
        let before = server.state().image_requests().len();
        upload(&api, &app, std::slice::from_ref(&image), &no_limit).unwrap();
        let state = server.state();
        assert!(
            state.image_requests()[before..]
                .iter()
                .all(|request| request.starts_with("HEAD")),
            "{:?}",
            state.image_requests()
        );
        drop(state);

        // Trop gros pour le serveur : refusé avant l'envoi.
        let other_dir = dir.path().join("oci2");
        oci_layout(&other_dir, &[("arm64", "")], "v2");
        let other = prepare("web", &other_dir, &[Arch::Aarch64], &dir.path().join("w2")).unwrap();
        let too_big = |path: &Path, _: u64| -> Result<()> { bail!("{} trop gros", path.display()) };
        let error = format!("{:#}", upload(&api, &app, &[other], &too_big).unwrap_err());
        assert!(error.contains("trop gros"), "{error}");
    }

    #[test]
    fn the_fake_server_refuses_like_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let oci_dir = dir.path().join("oci");
        oci_layout(&oci_dir, &[("arm64", "")], "v1");
        let image = prepare("web", &oci_dir, &[Arch::Aarch64], &dir.path().join("w")).unwrap();
        let (server, api, app) = stack_server();
        let platform = &image.platforms[0];
        let code = |error: crate::api::ApiError| error.code().unwrap_or("aucun").to_owned();

        // Un manifeste dont les couches manquent, un index dont le manifeste manque.
        let error = api
            .put_manifest(
                &app,
                &platform.manifest.digest,
                OCI_MANIFEST,
                &platform.manifest.body,
            )
            .unwrap_err();
        assert!(error.to_string().contains("absente du serveur"), "{error}");
        assert_eq!(code(error), "artifact_invalid");
        let error = api
            .put_manifest(&app, &image.index.digest, OCI_INDEX, &image.index.body)
            .unwrap_err();
        assert_eq!(code(error), "artifact_invalid");
        // Un autre type, une autre empreinte, un autre contenu.
        let error = api
            .put_manifest(
                &app,
                &image.index.digest,
                "application/json",
                &image.index.body,
            )
            .unwrap_err();
        assert_eq!(code(error), "unsupported_media_type");
        let error = api
            .put_blob(
                &app,
                &platform.layers[0].digest,
                &platform.config.path,
                platform.config.size,
                "x",
            )
            .unwrap_err();
        assert_eq!(code(error), "artifact_invalid");
        assert_eq!(api.blob_size(&app, &platform.config.digest).unwrap(), None);
        assert!(
            api.put_blob(
                &app,
                &platform.config.digest,
                &platform.config.path,
                platform.config.size,
                "x"
            )
            .unwrap()
        );
        assert_eq!(
            api.blob_size(&app, &platform.config.digest).unwrap(),
            Some(platform.config.size)
        );
        // Déjà là : 200, pas 201.
        assert!(
            !api.put_blob(
                &app,
                &platform.config.digest,
                &platform.config.path,
                platform.config.size,
                "x"
            )
            .unwrap()
        );

        // Une app d'un autre type ne reçoit pas d'images.
        server.state().apps.get_mut("test-app").unwrap().kind = "swift".to_owned();
        let error = api.blob_size(&app, &platform.config.digest).unwrap_err();
        assert_eq!(code(error), "invalid_request");
        let error = api
            .put_manifest(&app, &image.index.digest, OCI_INDEX, &image.index.body)
            .unwrap_err();
        assert_eq!(code(error), "invalid_request");
    }

    #[test]
    fn a_single_platform_build_without_index_is_read_from_its_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let oci_dir = dir.path().join("oci");
        oci_layout(&oci_dir, &[("amd64", "")], "v1");
        // Le dossier ne désigne que le manifeste, comme une construction pour une seule plateforme.
        let layout = Layout {
            dir: oci_dir.clone(),
        };
        let manifest = image_manifests(&layout).unwrap().remove(0);
        fs::write(
            oci_dir.join("index.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "manifests": [{"mediaType": OCI_MANIFEST, "digest": manifest.digest, "size": 1}],
            }))
            .unwrap(),
        )
        .unwrap();
        let image = prepare("web", &oci_dir, &[Arch::X86_64], &dir.path().join("w")).unwrap();
        assert_eq!(image.platforms.len(), 1);
        assert_eq!(image.platforms[0].arch, Arch::X86_64);
        assert!(matches!(reread(&image.index), OciObject::Index(_)));
    }
}
