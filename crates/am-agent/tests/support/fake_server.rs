//! Faux serveur de mises à jour, en mémoire, pour les essais de l'agent. Il ne sert que ce que
//! l'agent demande, et sait mentir comme un serveur incohérent ne le devrait pas : taille ou
//! SHA-256 faux, version proposée à un client qui l'a déjà.
//!
//! Il suit docs/PROTOCOLE.md, « Vérification » et « Téléchargement » : il propose la plus haute
//! version publiée, d'un canal permis, strictement supérieure à celle du client, pour l'app et
//! l'arch demandées, puis sert son artefact par `/dl/<id>/<fichier>`. Il retient chaque
//! vérification (route, paramètres, identifiant d'installation) et chaque téléchargement.
//!
//! Pour l'essai sous Linux (`examples/trial_server.rs`), il répond aussi comme le registre en
//! lecture seule de « Registre » : `/v2/`, puis manifestes et couches par empreinte, lus dans un
//! dossier OCI (`blobs/sha256/<hex>`).
//!
//! Ce fichier est inclus par les tests unitaires du binaire, par les tests d'intégration et par
//! l'exemple : chacun n'en emploie qu'une partie.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use am_protocol::update::{HEADER_INSTALL_ID, UpdateResponse};
use am_protocol::{Arch, BundleType, Channel, Version};
use axum::extract::{Path as UrlPath, Query, State as Shared};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use sha2::{Digest, Sha256};

/// Une version publiée, avec son unique artefact.
#[derive(Clone, Debug)]
pub struct FakeRelease {
    pub app: String,
    pub version: Version,
    pub channel: Channel,
    pub arch: Arch,
    pub bundle_type: BundleType,
    pub bytes: Vec<u8>,
    pub signature: String,
    pub file_name: String,
    pub notes: Option<String>,
    /// Proposée quelle que soit la version ou le canal du client : un serveur incohérent.
    pub always: bool,
    /// Taille et SHA-256 annoncés, s'ils diffèrent des vrais.
    pub announced_size: Option<u64>,
    pub announced_sha256: Option<String>,
}

impl FakeRelease {
    pub fn stack(app: &str, version: &str, arch: Arch, bytes: Vec<u8>, signature: String) -> Self {
        Self::new(app, version, arch, BundleType::Stack, bytes, signature)
    }

    pub fn bin(app: &str, version: &str, arch: Arch, bytes: Vec<u8>, signature: String) -> Self {
        Self::new(app, version, arch, BundleType::Bin, bytes, signature)
    }

    fn new(
        app: &str,
        version: &str,
        arch: Arch,
        bundle_type: BundleType,
        bytes: Vec<u8>,
        signature: String,
    ) -> Self {
        let version = Version::parse_release(version).unwrap();
        let file_name = match bundle_type {
            BundleType::Stack => format!("{app}-{version}-{arch}.stack.tar.gz"),
            _ => format!("{app}-{version}-{arch}"),
        };
        Self {
            app: app.to_owned(),
            version,
            channel: Channel::Stable,
            arch,
            bundle_type,
            bytes,
            signature,
            file_name,
            notes: None,
            always: false,
            announced_size: None,
            announced_sha256: None,
        }
    }

    pub fn beta(mut self) -> Self {
        self.channel = Channel::Beta;
        self
    }

    pub fn always(mut self) -> Self {
        self.always = true;
        self
    }

    pub fn notes(mut self, notes: &str) -> Self {
        self.notes = Some(notes.to_owned());
        self
    }
}

/// Une vérification reçue.
#[derive(Clone, Debug)]
pub struct CheckRecord {
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub install_id: Option<String>,
}

#[derive(Default)]
struct State {
    url: String,
    /// Apps connues du serveur : déclarées, ou dont une version a été publiée.
    apps: std::collections::BTreeSet<String>,
    releases: Vec<FakeRelease>,
    checks: Vec<CheckRecord>,
    downloads: Vec<String>,
    /// Dossier OCI servi en registre, par app.
    registries: BTreeMap<String, PathBuf>,
}

type SharedState = Arc<Mutex<State>>;

fn lock(state: &SharedState) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FakeServer {
    /// Adresse du serveur, par exemple `http://127.0.0.1:54321`.
    pub url: String,
    state: SharedState,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl FakeServer {
    /// Démarre sur un port libre de la boucle locale, dans un fil à part.
    pub fn start() -> Self {
        Self::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    pub fn bind(address: SocketAddr) -> Self {
        let listener = std::net::TcpListener::bind(address)
            .unwrap_or_else(|error| panic!("écoute sur {address} : {error}"));
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            url: url.clone(),
            ..State::default()
        }));
        let app = Router::new()
            .route("/api/v1/update/{app}/{target}/{arch}/{version}", get(check))
            .route("/dl/{id}/{file}", get(download))
            .route("/v2/", get(registry_root))
            .route("/v2", get(registry_root))
            .route("/v2/{app}/{image}/manifests/{digest}", get(manifest))
            .route("/v2/{app}/{image}/blobs/{digest}", get(blob))
            .with_state(Arc::clone(&state));
        let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stop.await;
                    })
                    .await
                    .unwrap();
            });
        });
        Self {
            url,
            state,
            shutdown: Some(shutdown),
        }
    }

    /// Déclare une app sans version publiée : sa vérification répond 204, pas 404.
    pub fn declare(&self, app: &str) {
        lock(&self.state).apps.insert(app.to_owned());
    }

    pub fn publish(&self, release: FakeRelease) {
        let mut state = lock(&self.state);
        state.apps.insert(release.app.clone());
        state.releases.push(release);
    }

    /// Retire toutes les versions publiées ; les apps restent connues.
    pub fn clear(&self) {
        lock(&self.state).releases.clear();
    }

    pub fn checks(&self) -> Vec<CheckRecord> {
        lock(&self.state).checks.clone()
    }

    /// Fichiers téléchargés, dans l'ordre.
    pub fn downloads(&self) -> Vec<String> {
        lock(&self.state).downloads.clone()
    }

    /// Adresse du serveur vue des clients, si ce n'est pas `url` : celle des téléchargements.
    pub fn set_public_url(&self, url: &str) {
        lock(&self.state).url = url.trim_end_matches('/').to_owned();
    }

    /// Sert en registre, pour `app`, les objets d'un dossier OCI (`blobs/sha256/<hex>`).
    pub fn serve_oci(&self, app: &str, layout: PathBuf) {
        lock(&self.state).registries.insert(app.to_owned(), layout);
    }
}

fn registry_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"errors": [{"code": code, "message": message, "detail": null}]})),
    )
        .into_response()
}

async fn registry_root() -> Response {
    (
        [("docker-distribution-api-version", "registry/2.0")],
        Json(serde_json::json!({})),
    )
        .into_response()
}

/// Objet d'un dossier OCI, par empreinte, pour une app servie en registre ; sinon le code et le
/// message d'erreur du registre.
fn registry_object(
    state: &SharedState,
    app: &str,
    digest: &str,
) -> Result<(String, Vec<u8>), (&'static str, &'static str)> {
    let layout = lock(state)
        .registries
        .get(app)
        .cloned()
        .ok_or(("NAME_UNKNOWN", "app inconnue"))?;
    let parsed = am_protocol::stack::Digest::parse(digest).map_err(|_| {
        (
            "MANIFEST_UNKNOWN",
            "objet désigné par une étiquette ou une empreinte illisible",
        )
    })?;
    let bytes = std::fs::read(layout.join("blobs/sha256").join(parsed.hex()))
        .map_err(|_| ("BLOB_UNKNOWN", "objet absent"))?;
    Ok((parsed.to_string(), bytes))
}

async fn manifest(
    Shared(state): Shared<SharedState>,
    UrlPath((app, _image, digest)): UrlPath<(String, String, String)>,
) -> Response {
    let (digest, bytes) = match registry_object(&state, &app, &digest) {
        Ok(object) => object,
        Err((code, message)) => return registry_error(StatusCode::NOT_FOUND, code, message),
    };
    let media_type = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("mediaType")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned());
    (
        [
            (header::CONTENT_TYPE, media_type),
            (
                header::HeaderName::from_static("docker-content-digest"),
                digest.clone(),
            ),
            (header::ETAG, format!("\"{digest}\"")),
        ],
        bytes,
    )
        .into_response()
}

async fn blob(
    Shared(state): Shared<SharedState>,
    UrlPath((app, _image, digest)): UrlPath<(String, String, String)>,
) -> Response {
    match registry_object(&state, &app, &digest) {
        Ok((digest, bytes)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (
                    header::HeaderName::from_static("docker-content-digest"),
                    digest.clone(),
                ),
                (header::ETAG, format!("\"{digest}\"")),
            ],
            bytes,
        )
            .into_response(),
        Err((code, message)) => registry_error(StatusCode::NOT_FOUND, code, message),
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn check(
    Shared(state): Shared<SharedState>,
    UrlPath((app, target, arch, version)): UrlPath<(String, String, String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut state = lock(&state);
    let install_id = headers
        .get(HEADER_INSTALL_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.checks.push(CheckRecord {
        path: format!("/api/v1/update/{app}/{target}/{arch}/{version}"),
        query: query.clone().into_iter().collect(),
        install_id,
    });
    let (Ok(arch), Ok(current)) = (Arch::parse_client(&arch), Version::parse_client(&version))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_request", "message": "requête illisible"})),
        )
            .into_response();
    };
    if target != "linux" || !state.apps.contains(&app) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found", "message": "app inconnue"})),
        )
            .into_response();
    }
    let channel = Channel::parse_client(query.get("channel").map(String::as_str));
    let chosen = state
        .releases
        .iter()
        .enumerate()
        .filter(|(_, release)| release.app == app && release.arch == arch)
        .filter(|(_, release)| {
            release.always || (release.version > current && channel.includes(release.channel))
        })
        .max_by(|(_, a), (_, b)| a.version.cmp(&b.version));
    let Some((index, release)) = chosen else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let sha256 = release
        .announced_sha256
        .clone()
        .unwrap_or_else(|| hex::encode(Sha256::digest(&release.bytes)));
    let response = UpdateResponse {
        version: release.version.clone(),
        url: format!("{}/dl/r{index}/{}", state.url, release.file_name),
        signature: release.signature.clone(),
        notes: release.notes.clone(),
        pub_date: "2026-11-12T10:00:00Z".to_owned(),
        size: release.announced_size.unwrap_or(release.bytes.len() as u64),
        sha256,
        min_os_version: None,
        channel: release.channel,
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response()
}

async fn download(
    Shared(state): Shared<SharedState>,
    UrlPath((id, file)): UrlPath<(String, String)>,
) -> Response {
    let mut state = lock(&state);
    let release = id
        .strip_prefix('r')
        .and_then(|index| index.parse::<usize>().ok())
        .and_then(|index| state.releases.get(index))
        .cloned();
    match release {
        Some(release) => {
            state.downloads.push(file);
            (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                release.bytes,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
