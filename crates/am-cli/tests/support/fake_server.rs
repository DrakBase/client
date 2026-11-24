//! Faux serveur de publication, en mémoire, pour les tests de l'outil et ses essais à la main
//! (`cargo run -p am-cli --example fake_server`) : il répond comme le vrai serveur
//! (`crates/am-server`) sur ce que l'outil lui demande, sans base ni disque, et laisse les tests
//! lire ce qu'il a reçu et provoquer des pannes.
//!
//! Il suit docs/API.md, « Publication par l'outil » et « Envoi d'un artefact » : jeton et
//! portées, taille maximale annoncée par `whoami`, version d'un brouillon lue en forme souple et
//! rendue canonique, identifiants des clés embarquées mis en majuscules et sans doublon, brouillon
//! repris avec le canal, les notes et les clés envoyés (sans `notes_md`, il garde ses notes),
//! refus d'un envoi avant la lecture de son corps puis lecture du reste, corps écrit en flux avec
//! SHA-256 et vérification minisign, une seule clé par version, retrait d'un artefact d'un
//! brouillon, et les contrôles de la publication (`check_publishable` du serveur) : un artefact de
//! mise à jour au moins, une seule clé, active ou suivante, et la version signée. Il garde aussi la
//! fiche publique de chaque app (« Captures d'écran ») : description, et captures contrôlées
//! comme sur le serveur (PNG, taille, dimensions, nombre), dédoublonnées par SHA-256, avec leur
//! légende et leur ordre. Il n'inspecte pas le contenu des archives macOS et ne garde rien sur le
//! disque.
//!
//! Pour les stacks Docker (docs/API.md, « Stacks Docker ») : couches et manifestes rangés par app
//! (`HEAD` et `PUT`), avec les refus du contrat (type d'app, empreinte, taille, type de manifeste,
//! objets désignés absents, plateforme hors de la table) ; archives `stack` lues comme sur le
//! serveur (règles de l'archive, `stack.json` publié, images présentes pour l'arch, mêmes images
//! dans toutes les archives, clés de `public_keys` devenues `trusted_key_ids`) ; exécutables
//! `bin` contrôlés par leur en-tête ELF : un exécutable, de l'arch de son emplacement.
//!
//! Ce fichier est inclus par les tests unitaires du binaire, par les tests d'intégration et par
//! l'exemple : chacun n'en emploie qu'une partie.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use am_protocol::elf::ELF_HEADER_LEN;
use am_protocol::listing::{
    MAX_CAPTION_CHARS, MAX_DESCRIPTION_CHARS, MAX_SCREENSHOT_BYTES, MAX_SCREENSHOTS,
    SCREENSHOT_SIDES, png_dimensions,
};
use am_protocol::publish::{
    AppInfo, Artifact, Creator, Key, Latest, MAX_NOTES_CHARS, NewRelease, Release,
    SCOPE_LISTING_WRITE as LISTING_SCOPE, SCOPE_RELEASES_PUBLISH as PUBLISH_SCOPE,
    SCOPE_RELEASES_WRITE as WRITE_SCOPE, Screenshot, WhoAmI, WhoAmIToken, WhoAmIUser,
};
use am_protocol::signature::{PublicKey, Signature};
use am_protocol::stack::manifest::StackManifest;
use am_protocol::stack::oci::{self, OciObject};
use am_protocol::stack::{Digest as StackDigest, archive as stack_archive};
use am_protocol::update::{HEADER_FILENAME, HEADER_SHA256, HEADER_SIGNATURE};
use am_protocol::{Arch, BundleType, Channel, Os, Version};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Request, State as Shared};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Après un refus anticipé, le reste du corps est lu pendant ce temps au plus (docs/API.md).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

type SharedState = Arc<Mutex<State>>;

pub struct FakeServer {
    /// Adresse du serveur, par exemple `http://127.0.0.1:54321`.
    pub url: String,
    state: SharedState,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl FakeServer {
    /// Démarre sur un port libre de la boucle locale, dans un fil à part.
    pub fn start(state: State) -> Self {
        Self::bind(SocketAddr::from(([127, 0, 0, 1], 0)), state)
    }

    pub fn bind(address: SocketAddr, mut state: State) -> Self {
        let listener = std::net::TcpListener::bind(address)
            .unwrap_or_else(|error| panic!("écoute sur {address} : {error}"));
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        state.url = url.clone();
        let state = Arc::new(Mutex::new(state));
        let app = router(Arc::clone(&state));
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

    pub fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn lock(state: &SharedState) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Tout ce que sait le faux serveur.
pub struct State {
    pub url: String,
    pub apps: BTreeMap<String, FakeApp>,
    pub tokens: Vec<FakeToken>,
    /// Requêtes reçues, dans l'ordre.
    pub requests: Vec<Logged>,
    /// Corps JSON reçus par `POST …/releases`, dans l'ordre : ce que l'outil demande pour créer
    /// ou reprendre un brouillon.
    pub release_bodies: Vec<Value>,
    /// Taille maximale d'un artefact, que `whoami` annonce.
    pub max_artifact_bytes: u64,
    /// Adresse publique que `whoami` annonce (`public_url`), quand elle diffère de celle du faux
    /// serveur.
    pub public_url: Option<String>,
    /// L'envoi de cet emplacement (`darwin/universal/dmg`, ou `blob` pour la prochaine couche
    /// d'image) coupe la connexion après ce nombre d'octets reçus, sans répondre ; une seule fois.
    pub cut_upload: Option<(String, u64)>,
    /// Le prochain envoi d'artefact est refusé avec ce statut, ce code et ce message avant la
    /// lecture de son corps, alors que l'outil l'envoie encore : la limite d'un proxy, par exemple.
    pub refuse_next_upload: Option<(u16, String, String)>,
    /// Le prochain envoi est refusé avec ce statut, ce code et ce message, une fois son corps lu.
    pub reject_next_upload: Option<(u16, String, String)>,
    /// Écrit chaque requête et sa réponse sur la sortie d'erreur, pour les essais à la main.
    pub verbose: bool,
    /// Couches et configurations d'images, par app puis par empreinte.
    pub blobs: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Manifestes et index, par app puis par empreinte.
    pub manifests: BTreeMap<String, BTreeMap<String, FakeManifest>>,
    next_id: u64,
}

/// Un manifeste d'image ou un index reçu.
#[derive(Clone)]
pub struct FakeManifest {
    pub media_type: String,
    pub body: Vec<u8>,
    /// Manifeste d'image : l'arch de sa configuration.
    pub arch: Option<Arch>,
    /// Index : les archs de ses manifestes.
    pub archs: Vec<Arch>,
}

impl State {
    /// L'app `test-app` (« Test App », Swift), dont la clé active est la clé d'essai principale de
    /// `fixtures/keys`, sans description ni capture, et trois jetons :
    /// - `amk_essai_complet` : `releases:write`, `releases:publish` et `listing:write` ;
    /// - `amk_essai_ecriture` : `releases:write` seulement ;
    /// - `amk_essai_autre_app` : les deux portées, mais pour l'app `autre` seulement.
    pub fn demo(bundle_id: Option<&str>) -> Self {
        let public_key = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/keys/test-app.pub"),
        )
        .unwrap();
        let mut apps = BTreeMap::new();
        apps.insert(
            "test-app".to_owned(),
            FakeApp {
                slug: "test-app".to_owned(),
                name: "Test App".to_owned(),
                kind: "swift".to_owned(),
                bundle_id: bundle_id.map(str::to_owned),
                keys: vec![FakeKey {
                    public_key: public_key.trim().to_owned(),
                    status: "active".to_owned(),
                }],
                releases: BTreeMap::new(),
                description: String::new(),
                screenshots: Vec::new(),
            },
        );
        let token = |secret: &str, name: &str, scopes: &[&str], app: Option<&str>| FakeToken {
            secret: secret.to_owned(),
            name: name.to_owned(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            app: app.map(str::to_owned),
            expires_at: "2027-10-25T00:00:00Z".to_owned(),
            user_email: "camille@example.fr".to_owned(),
            user_name: "Camille Essai".to_owned(),
            role: "editor".to_owned(),
        };
        Self {
            url: String::new(),
            apps,
            tokens: vec![
                token(
                    "amk_essai_complet",
                    "Mac d'essai",
                    &[WRITE_SCOPE, PUBLISH_SCOPE, LISTING_SCOPE],
                    None,
                ),
                token("amk_essai_ecriture", "Écriture seule", &[WRITE_SCOPE], None),
                token(
                    "amk_essai_autre_app",
                    "Autre app",
                    &[WRITE_SCOPE, PUBLISH_SCOPE],
                    Some("autre"),
                ),
            ],
            requests: Vec::new(),
            release_bodies: Vec::new(),
            max_artifact_bytes: 1024 * 1024 * 1024,
            public_url: None,
            cut_upload: None,
            refuse_next_upload: None,
            reject_next_upload: None,
            verbose: false,
            blobs: BTreeMap::new(),
            manifests: BTreeMap::new(),
            next_id: 0,
        }
    }

    /// Ajoute une app de ce type (`swift`, `tauri`, `stack` ou `cli`), sans identifiant de bundle,
    /// avec cette clé active.
    pub fn add_app(&mut self, slug: &str, name: &str, kind: &str, public_key: &str) {
        self.apps.insert(
            slug.to_owned(),
            FakeApp {
                slug: slug.to_owned(),
                name: name.to_owned(),
                kind: kind.to_owned(),
                bundle_id: None,
                keys: vec![FakeKey {
                    public_key: public_key.trim().to_owned(),
                    status: "active".to_owned(),
                }],
                releases: BTreeMap::new(),
                description: String::new(),
                screenshots: Vec::new(),
            },
        );
    }

    /// Les requêtes reçues par ces routes, dans l'ordre : `PUT blobs`, `HEAD manifests`…
    pub fn image_requests(&self) -> Vec<String> {
        self.requests
            .iter()
            .filter_map(|request| {
                let rest = request.path.strip_prefix("/api/v1/publish/apps/")?;
                let (_, rest) = rest.split_once('/')?;
                let kind = rest.split('/').next()?;
                matches!(kind, "blobs" | "manifests").then(|| format!("{} {kind}", request.method))
            })
            .collect()
    }

    /// Ajoute une version déjà publiée, sans artefact, comme dernière version de son canal.
    pub fn publish_version(&mut self, app: &str, version: &str, channel: Channel) {
        let version = Version::parse_release(version).unwrap();
        self.apps.get_mut(app).unwrap().releases.insert(
            version.to_string(),
            FakeRelease {
                version,
                channel,
                status: "published".to_owned(),
                notes_md: String::new(),
                trusted_key_ids: Vec::new(),
                artifacts: Vec::new(),
                published_at: Some("2026-10-01T12:00:00Z".to_owned()),
                images: BTreeMap::new(),
            },
        );
    }

    /// Ajoute une capture à la fiche d'une app, sans passer par l'API ; rend son identifiant.
    pub fn add_screenshot(&mut self, app: &str, png: &[u8], caption: &str) -> String {
        let (width, height) = png_dimensions(png).unwrap();
        let id = self.new_id();
        self.apps
            .get_mut(app)
            .unwrap()
            .screenshots
            .push(FakeScreenshot {
                id: id.clone(),
                width,
                height,
                size: png.len() as u64,
                sha256: hex::encode(Sha256::digest(png)),
                caption: caption.to_owned(),
            });
        id
    }

    /// Les requêtes qui modifient la fiche publique d'une app : sa description ou ses captures.
    pub fn listing_changes(&self) -> Vec<&Logged> {
        self.requests
            .iter()
            .filter(|request| {
                let path = request.path.strip_prefix("/api/v1/publish/apps/");
                request.method != "GET"
                    && path.is_some_and(|rest| !rest.contains('/') || rest.contains("/screenshots"))
            })
            .collect()
    }

    /// Les envois reçus (requêtes `PUT`).
    pub fn uploads(&self) -> Vec<&Logged> {
        self.requests
            .iter()
            .filter(|request| request.method == "PUT")
            .collect()
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        // 26 caractères, comme un ULID.
        format!("01FAKE{:020}", self.next_id)
    }
}

pub struct FakeApp {
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub bundle_id: Option<String>,
    pub keys: Vec<FakeKey>,
    /// Par version canonique.
    pub releases: BTreeMap<String, FakeRelease>,
    /// Description de la fiche publique.
    pub description: String,
    /// Captures de la fiche publique, dans l'ordre d'affichage.
    pub screenshots: Vec<FakeScreenshot>,
}

pub struct FakeScreenshot {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub sha256: String,
    pub caption: String,
}

pub struct FakeKey {
    pub public_key: String,
    /// `active`, `next` ou `retired`.
    pub status: String,
}

impl FakeKey {
    fn key_id(&self) -> String {
        PublicKey::from_tauri(&self.public_key)
            .unwrap()
            .key_id()
            .to_hex()
    }
}

pub struct FakeRelease {
    pub version: Version,
    pub channel: Channel,
    /// `draft`, `published` ou `yanked`.
    pub status: String,
    pub notes_md: String,
    pub trusted_key_ids: Vec<String>,
    pub artifacts: Vec<FakeArtifact>,
    pub published_at: Option<String>,
    /// Images d'une stack, relevées à l'envoi de ses archives : nom → empreinte.
    pub images: BTreeMap<String, String>,
}

pub struct FakeArtifact {
    pub id: String,
    pub os: Os,
    pub arch: Arch,
    pub bundle_type: BundleType,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub signed_key_id: Option<String>,
    pub signed_version: Option<String>,
}

impl FakeArtifact {
    pub fn slot(&self) -> String {
        format!("{}/{}/{}", self.os, self.arch, self.bundle_type)
    }
}

#[derive(Clone)]
pub struct FakeToken {
    pub secret: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub app: Option<String>,
    pub expires_at: String,
    pub user_email: String,
    pub user_name: String,
    pub role: String,
}

/// Une requête reçue.
pub struct Logged {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
}

impl Logged {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/v1/publish/whoami", get(whoami))
        .route(
            "/api/v1/publish/apps/{app}",
            get(app_info).patch(update_app),
        )
        .route(
            "/api/v1/publish/apps/{app}/screenshots",
            post(add_screenshot),
        )
        .route(
            "/api/v1/publish/apps/{app}/screenshots/order",
            put(order_screenshots),
        )
        .route(
            "/api/v1/publish/apps/{app}/screenshots/{id}",
            patch(set_caption).delete(delete_screenshot),
        )
        .route("/api/v1/publish/apps/{app}/releases", post(create_release))
        .route(
            "/api/v1/publish/apps/{app}/releases/{version}/publish",
            post(publish_release),
        )
        .route(
            "/api/v1/publish/apps/{app}/releases/{version}/artifacts/{os}/{arch}/{bundle_type}",
            put(upload),
        )
        .route(
            "/api/v1/publish/apps/{app}/artifacts/{artifact_id}",
            delete(delete_artifact),
        )
        .route(
            "/api/v1/publish/apps/{app}/blobs/{digest}",
            put(put_blob).head(head_blob),
        )
        .route(
            "/api/v1/publish/apps/{app}/manifests/{digest}",
            put(put_manifest).head(head_manifest),
        )
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(Arc::clone(&state), record))
        .with_state(state)
}

/// Garde chaque requête pour les tests, et l'écrit avec sa réponse en mode bavard.
async fn record(Shared(state): Shared<SharedState>, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let verbose = {
        let mut state = lock(&state);
        state.requests.push(Logged {
            method: method.clone(),
            path: path.clone(),
            headers: request.headers().clone(),
        });
        state.verbose
    };
    let response = next.run(request).await;
    if verbose {
        eprintln!("{method} {path} → {}", response.status().as_u16());
    }
    response
}

/// Un refus : un statut et le corps commun `{"error", "message"}` (docs/API.md).
pub struct Refusal {
    status: StatusCode,
    code: String,
    message: String,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": self.code, "message": self.message})),
        )
            .into_response()
    }
}

fn refuse(status: StatusCode, code: &str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code: code.to_owned(),
        message: message.into(),
    }
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    refuse(status, code, message).into_response()
}

/// Le jeton de la requête, s'il a la portée demandée et sert à cette app.
fn authorize(
    state: &State,
    headers: &HeaderMap,
    scope: Option<&str>,
    app: Option<&str>,
) -> Result<FakeToken, Refusal> {
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) =
        secret.and_then(|secret| state.tokens.iter().find(|token| token.secret == secret))
    else {
        return Err(refuse(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "jeton absent, inconnu, révoqué ou expiré",
        ));
    };
    if let Some(scope) = scope
        && !token.scopes.iter().any(|granted| granted == scope)
    {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!("ce jeton n'a pas la portée {scope}"),
        ));
    }
    if let (Some(only), Some(app)) = (&token.app, app)
        && only != app
    {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!("ce jeton ne sert qu'à l'app {only}"),
        ));
    }
    Ok(token.clone())
}

/// Corps JSON d'une requête qui modifie : `Content-Type: application/json` exigé.
fn json_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, Refusal> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return Err(refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "corps JSON attendu",
        ));
    }
    serde_json::from_slice(body)
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, "invalid_request", "JSON illisible"))
}

async fn whoami(Shared(state): Shared<SharedState>, headers: HeaderMap) -> Response {
    let state = lock(&state);
    match authorize(&state, &headers, None, None) {
        Err(refusal) => refusal.into_response(),
        Ok(token) => Json(WhoAmI {
            user: WhoAmIUser {
                email: token.user_email,
                name: token.user_name,
                role: token.role,
            },
            token: WhoAmIToken {
                name: token.name,
                scopes: token.scopes,
                app: token.app,
                expires_at: token.expires_at,
            },
            max_artifact_bytes: state.max_artifact_bytes,
            public_url: state.public_url.clone().unwrap_or(state.url.clone()),
        })
        .into_response(),
    }
}

async fn app_info(
    Shared(state): Shared<SharedState>,
    UrlPath(slug): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, None, Some(&slug)) {
        return refusal.into_response();
    }
    match state.apps.get(&slug) {
        Some(app) => Json(app_json(app, &state.url)).into_response(),
        None => unknown_app(&slug),
    }
}

fn unknown_release(version: &Version) -> Refusal {
    refuse(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("version {version} inconnue"),
    )
}

fn unknown_app(slug: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("app {slug} inconnue"),
    )
}

fn invalid(message: impl Into<String>) -> Response {
    error(StatusCode::BAD_REQUEST, "invalid_request", message)
}

/// L'app telle que la rend `GET /api/v1/publish/apps/{app}`.
fn app_json(app: &FakeApp, base: &str) -> AppInfo {
    // Plus haute version publiée de chaque canal.
    let latest = |channel: Channel| {
        app.releases
            .values()
            .filter(|release| release.status == "published" && release.channel == channel)
            .map(|release| release.version.clone())
            .max()
    };
    AppInfo {
        slug: app.slug.clone(),
        name: app.name.clone(),
        kind: app.kind.parse().unwrap(),
        bundle_id: app.bundle_id.clone(),
        description: app.description.clone(),
        screenshots: screenshots_json(app, base),
        keys: app
            .keys
            .iter()
            .map(|key| Key {
                key_id: key.key_id(),
                public_key: key.public_key.clone(),
                status: key.status.parse().unwrap(),
                created_at: "2026-10-01T12:00:00Z".to_owned(),
            })
            .collect(),
        latest: Latest {
            stable: latest(Channel::Stable),
            beta: latest(Channel::Beta),
        },
    }
}

fn screenshot_json(app: &FakeApp, screenshot: &FakeScreenshot, base: &str) -> Screenshot {
    Screenshot {
        id: screenshot.id.clone(),
        url: format!(
            "{base}/api/admin/apps/{}/screenshots/{}",
            app.slug, screenshot.id
        ),
        width: screenshot.width,
        height: screenshot.height,
        size: screenshot.size,
        sha256: screenshot.sha256.clone(),
        caption: screenshot.caption.clone(),
        created_at: "2026-10-29T08:00:00Z".to_owned(),
    }
}

fn screenshots_json(app: &FakeApp, base: &str) -> Vec<Screenshot> {
    app.screenshots
        .iter()
        .map(|screenshot| screenshot_json(app, screenshot, base))
        .collect()
}

/// `PATCH …/apps/{app}` `{description}` : la description de la fiche publique.
async fn update_app(
    Shared(state): Shared<SharedState>,
    UrlPath(slug): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(LISTING_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let body = match json_body(&headers, &body) {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    if let Some(description) = body.get("description") {
        let Some(description) = description.as_str().map(str::trim) else {
            return invalid("description : texte attendu");
        };
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return invalid(format!(
                "description : {MAX_DESCRIPTION_CHARS} caractères au plus"
            ));
        }
        app.description = description.to_owned();
    }
    Json(app_json(app, &base)).into_response()
}

/// `POST …/screenshots`, corps `image/png` : droits, type et taille annoncée sont contrôlés
/// avant de lire le corps ; 200 et la capture existante si l'app a déjà cette image.
async fn add_screenshot(
    Shared(state): Shared<SharedState>,
    UrlPath(slug): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let mut stream = body.into_data_stream();
    let checked = {
        let state = lock(&state);
        expect_screenshot(&state, &headers, &slug)
    };
    if let Err(refusal) = checked {
        drain(stream);
        return refusal.into_response();
    }
    let mut png = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return invalid("corps interrompu");
        };
        png.extend_from_slice(&chunk);
        if png.len() as u64 > MAX_SCREENSHOT_BYTES {
            drain(stream);
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "capture de 8 Mio au plus",
            );
        }
    }
    let Some((width, height)) = png_dimensions(&png) else {
        return invalid("la capture doit être une image PNG lisible");
    };
    if !SCREENSHOT_SIDES.contains(&width) || !SCREENSHOT_SIDES.contains(&height) {
        return invalid(format!(
            "capture de {width} × {height} pixels : de 320 à 8192 pixels de côté"
        ));
    }
    let sha256 = hex::encode(Sha256::digest(&png));
    let mut state = lock(&state);
    let id = state.new_id();
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    if let Some(existing) = app
        .screenshots
        .iter()
        .find(|screenshot| screenshot.sha256 == sha256)
    {
        return (StatusCode::OK, Json(screenshot_json(app, existing, &base))).into_response();
    }
    if app.screenshots.len() >= MAX_SCREENSHOTS {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!("{MAX_SCREENSHOTS} captures par app au plus"),
        );
    }
    app.screenshots.push(FakeScreenshot {
        id,
        width,
        height,
        size: png.len() as u64,
        sha256,
        caption: String::new(),
    });
    let created = screenshot_json(app, &app.screenshots[app.screenshots.len() - 1], &base);
    (StatusCode::CREATED, Json(created)).into_response()
}

/// Portée, app, type et taille annoncée d'une capture, avant la lecture du corps.
fn expect_screenshot(state: &State, headers: &HeaderMap, slug: &str) -> Result<(), Refusal> {
    authorize(state, headers, Some(LISTING_SCOPE), Some(slug))?;
    if !state.apps.contains_key(slug) {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("app {slug} inconnue"),
        ));
    }
    let text = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let is_png = text(header::CONTENT_TYPE)
        .is_some_and(|value| value.split(';').next().unwrap_or_default().trim() == "image/png");
    if !is_png {
        return Err(refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "capture en PNG attendue (Content-Type: image/png)",
        ));
    }
    let declared = text(header::CONTENT_LENGTH).and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_SCREENSHOT_BYTES) {
        return Err(refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "capture de 8 Mio au plus",
        ));
    }
    Ok(())
}

/// `PATCH …/screenshots/{id}` `{caption}`.
async fn set_caption(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(LISTING_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let body = match json_body(&headers, &body) {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    let Some(caption) = body["caption"].as_str().map(str::trim) else {
        return invalid("caption : texte attendu");
    };
    if caption.chars().count() > MAX_CAPTION_CHARS {
        return invalid(format!("légende : {MAX_CAPTION_CHARS} caractères au plus"));
    }
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    let Some(index) = app
        .screenshots
        .iter()
        .position(|screenshot| screenshot.id == id)
    else {
        return error(StatusCode::NOT_FOUND, "not_found", "capture inconnue");
    };
    app.screenshots[index].caption = caption.to_owned();
    Json(screenshot_json(app, &app.screenshots[index], &base)).into_response()
}

/// `PUT …/screenshots/order` `{ids}` : toutes les captures de l'app, chacune une fois.
async fn order_screenshots(
    Shared(state): Shared<SharedState>,
    UrlPath(slug): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(LISTING_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let body = match json_body(&headers, &body) {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    let Some(ids) = body["ids"].as_array().and_then(|ids| {
        ids.iter()
            .map(|id| id.as_str().map(str::to_owned))
            .collect::<Option<Vec<String>>>()
    }) else {
        return invalid("ids : tableau d'identifiants attendu");
    };
    let mut wanted: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut current: Vec<&str> = app
        .screenshots
        .iter()
        .map(|screenshot| screenshot.id.as_str())
        .collect();
    wanted.sort_unstable();
    current.sort_unstable();
    if wanted != current {
        return invalid("ids : toutes les captures de l'app, chacune une fois");
    }
    app.screenshots.sort_by_key(|screenshot| {
        ids.iter()
            .position(|id| *id == screenshot.id)
            .unwrap_or_default()
    });
    Json(screenshots_json(app, &base)).into_response()
}

async fn delete_screenshot(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(LISTING_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    let before = app.screenshots.len();
    app.screenshots.retain(|screenshot| screenshot.id != id);
    if app.screenshots.len() == before {
        return error(StatusCode::NOT_FOUND, "not_found", "capture inconnue");
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Lit et jette le reste du corps d'une requête refusée, en tâche de fond : sinon la connexion
/// serait coupée avant que le client lise la réponse (docs/API.md).
fn drain(mut stream: axum::body::BodyDataStream) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
            while let Some(Ok(_)) = stream.next().await {}
        })
        .await;
    });
}

fn release_json(release: &FakeRelease, base: &str) -> Release {
    Release {
        version: release.version.clone(),
        channel: release.channel,
        status: release.status.parse().unwrap(),
        notes_md: release.notes_md.clone(),
        trusted_key_ids: release.trusted_key_ids.clone(),
        created_by: Some(Creator {
            id: 1,
            name: "Camille Essai".to_owned(),
            email: "camille@example.fr".to_owned(),
        }),
        created_at: "2026-10-25T12:00:00Z".to_owned(),
        published_at: release.published_at.clone(),
        yanked_at: None,
        yank_reason: None,
        artifacts: release
            .artifacts
            .iter()
            .map(|artifact| artifact_json(artifact, base))
            .collect(),
        images: None,
    }
}

fn artifact_json(artifact: &FakeArtifact, base: &str) -> Artifact {
    Artifact {
        id: artifact.id.clone(),
        os: artifact.os,
        arch: artifact.arch,
        bundle_type: artifact.bundle_type,
        filename: artifact.filename.clone(),
        size: artifact.size,
        sha256: artifact.sha256.clone(),
        signed_key_id: artifact.signed_key_id.clone(),
        signed_version: artifact.signed_version.clone(),
        min_os_version: None,
        url: format!("{base}/dl/{}/{}", artifact.id, artifact.filename),
        created_at: "2026-10-25T12:00:00Z".to_owned(),
    }
}

/// Version lue dans un corps ou un chemin, comme sur le serveur : forme souple (`v1.2`), rendue
/// canonique (`1.2.0`).
fn parse_version(input: &str) -> Result<Version, Refusal> {
    Version::parse_release(input).map_err(|problem| {
        refuse(
            StatusCode::BAD_REQUEST,
            "version_invalid",
            problem.to_string(),
        )
    })
}

/// Identifiants de clés embarquées, comme sur le serveur : 16 caractères hexadécimaux, mis en
/// majuscules, sans doublon.
fn parse_key_ids(key_ids: &[String]) -> Result<Vec<String>, Refusal> {
    let mut parsed: Vec<String> = Vec::new();
    for key_id in key_ids {
        let normalized = key_id.trim().to_ascii_uppercase();
        if normalized.len() != 16 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(refuse(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("identifiant de clé « {key_id} » : 16 caractères hexadécimaux attendus"),
            ));
        }
        if !parsed.contains(&normalized) {
            parsed.push(normalized);
        }
    }
    Ok(parsed)
}

async fn create_release(
    Shared(state): Shared<SharedState>,
    UrlPath(slug): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(WRITE_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let body = match json_body(&headers, &body) {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    state.release_bodies.push(body.clone());
    // `channel`, `notes_md` et `trusted_key_ids` sont facultatifs, et `null` vaut absent. Absents,
    // un nouveau brouillon est sur le canal stable, sans notes ni clés déclarées, et un brouillon
    // existant garde les siens ; présents, même vides, ils les remplacent.
    let Ok(new) = serde_json::from_value::<NewRelease>(body) else {
        return invalid("version, canal (stable ou beta), notes ou clés illisibles");
    };
    let version = match parse_version(&new.version) {
        Ok(version) => version,
        Err(refusal) => return refusal.into_response(),
    };
    let (channel, notes) = (new.channel, new.notes_md);
    if notes
        .as_ref()
        .is_some_and(|notes| notes.chars().count() > MAX_NOTES_CHARS)
    {
        return invalid(format!(
            "notes trop longues : {MAX_NOTES_CHARS} caractères au plus"
        ));
    }
    let trusted = match new
        .trusted_key_ids
        .as_deref()
        .map(parse_key_ids)
        .transpose()
    {
        Ok(trusted) => trusted,
        Err(refusal) => return refusal.into_response(),
    };
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("app {slug} inconnue"),
        );
    };
    let key = version.to_string();
    if let Some(existing) = app.releases.get_mut(&key) {
        if existing.status != "draft" {
            return error(
                StatusCode::CONFLICT,
                "conflict",
                format!("la version {key} existe déjà ({})", existing.status),
            );
        }
        // Même brouillon : il prend le canal, les notes et les clés envoyés (docs/API.md,
        // « Brouillon existant »), ce qui permet à l'outil de reprendre une publication.
        if let Some(channel) = channel {
            existing.channel = channel;
        }
        if let Some(notes) = notes {
            existing.notes_md = notes;
        }
        if let Some(trusted) = trusted {
            existing.trusted_key_ids = trusted;
        }
        return (StatusCode::OK, Json(release_json(existing, &base))).into_response();
    }
    let release = FakeRelease {
        version,
        channel: channel.unwrap_or_default(),
        status: "draft".to_owned(),
        notes_md: notes.unwrap_or_default(),
        trusted_key_ids: trusted.unwrap_or_default(),
        artifacts: Vec::new(),
        published_at: None,
        images: BTreeMap::new(),
    };
    let reply = release_json(&release, &base);
    app.releases.insert(key, release);
    (StatusCode::CREATED, Json(reply)).into_response()
}

async fn publish_release(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, version)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(PUBLISH_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let version = match parse_version(&version) {
        Ok(version) => version,
        Err(refusal) => return refusal.into_response(),
    };
    let base = state.url.clone();
    let Some(app) = state.apps.get_mut(&slug) else {
        return unknown_app(&slug);
    };
    let FakeApp { keys, releases, .. } = app;
    let Some(release) = releases.get_mut(&version.to_string()) else {
        return unknown_release(&version).into_response();
    };
    let conflict = |message: String| error(StatusCode::CONFLICT, "conflict", message);
    match release.status.as_str() {
        "draft" => {}
        "published" => return conflict(format!("la version {version} est déjà publiée")),
        _ => {
            return conflict(format!(
                "la version {version} est retirée : remets-la avec unyank"
            ));
        }
    }
    if let Err(problem) = check_publishable(keys, release) {
        return conflict(problem);
    }
    release.status = "published".to_owned();
    release.published_at = Some("2026-10-25T12:30:00Z".to_owned());
    Json(release_json(release, &base)).into_response()
}

/// Les contrôles de la publication, ceux du serveur (`check_publishable`) : au moins un artefact
/// de mise à jour, une seule clé, active ou suivante, et la version signée partout égale à la
/// version. Rend le message du refus.
fn check_publishable(keys: &[FakeKey], release: &FakeRelease) -> Result<(), String> {
    if !release
        .artifacts
        .iter()
        .any(|artifact| artifact.bundle_type.is_update_payload())
    {
        return Err(
            "aucun artefact de mise à jour : envoie au moins une archive (le dmg ne sert qu'à la première installation)"
                .to_owned(),
        );
    }
    let signing: BTreeSet<&str> = release
        .artifacts
        .iter()
        .filter_map(|artifact| artifact.signed_key_id.as_deref())
        .collect();
    if signing.len() > 1 {
        return Err(format!(
            "les artefacts sont signés par plusieurs clés ({}) : une seule est permise",
            signing.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    for key_id in signing {
        let usable = keys
            .iter()
            .any(|key| key.key_id() == key_id && key.status != "retired");
        if !usable {
            return Err(format!(
                "la clé {key_id}, qui signe les artefacts, n'est ni active ni suivante : renvoie des artefacts signés avec la clé active"
            ));
        }
    }
    // Un artefact signé l'est par une clé : `signed_key_id` tient lieu de sa signature.
    let expected = release.version.to_string();
    if let Some(artifact) = release.artifacts.iter().find(|artifact| {
        artifact.signed_key_id.is_some() && artifact.signed_version.as_deref() != Some(&expected)
    }) {
        return Err(format!(
            "l'artefact {} n'est pas signé pour la version {expected}",
            artifact.id
        ));
    }
    Ok(())
}

/// Retire un artefact d'un brouillon ; celui d'une version publiée reste (409).
async fn delete_artifact(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, artifact_id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut state = lock(&state);
    if let Err(refusal) = authorize(&state, &headers, Some(WRITE_SCOPE), Some(&slug)) {
        return refusal.into_response();
    }
    let Some(release) = state.apps.get_mut(&slug).and_then(|app| {
        app.releases.values_mut().find(|release| {
            release
                .artifacts
                .iter()
                .any(|artifact| artifact.id == artifact_id)
        })
    }) else {
        return error(StatusCode::NOT_FOUND, "not_found", "artefact inconnu");
    };
    if release.status != "draft" {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            format!(
                "la version {} n'est plus un brouillon : ses artefacts ne se suppriment plus que dans l'administration",
                release.version
            ),
        );
    }
    release
        .artifacts
        .retain(|artifact| artifact.id != artifact_id);
    StatusCode::NO_CONTENT.into_response()
}

/// Ce que l'envoi doit être, contrôlé avant de lire le corps.
struct Expected {
    slug: String,
    version: Version,
    os: Os,
    arch: Arch,
    bundle_type: BundleType,
    filename: String,
    length: u64,
    signature: Option<(Signature, PublicKey)>,
    sha256: Option<String>,
    cut_after: Option<u64>,
    reject: Option<(u16, String, String)>,
}

async fn upload(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, version, os, arch, bundle_type)): UrlPath<(
        String,
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let expected = {
        let mut state = lock(&state);
        expect_upload(
            &mut state,
            &headers,
            slug,
            &version,
            &os,
            &arch,
            &bundle_type,
        )
    };
    let expected = match expected {
        Ok(expected) => expected,
        Err(refusal) => {
            // Refus avant la lecture du corps : le reste est lu en tâche de fond, sinon la
            // connexion serait coupée avant que le client lise la réponse (docs/API.md).
            drain(body.into_data_stream());
            return refusal.into_response();
        }
    };

    let mut hasher = Sha256::new();
    let mut verifier = match &expected.signature {
        Some((signature, key)) => match key.verify_stream(signature) {
            Ok(verifier) => Some(verifier),
            Err(problem) => {
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "signature_invalid",
                    format!("signature refusée : {problem}"),
                );
            }
        },
        None => None,
    };
    let mut received = 0u64;
    // Une archive de stack est lue en entier ; d'un exécutable, il suffit de l'en-tête.
    let mut kept = Vec::new();
    let keep = match expected.bundle_type {
        BundleType::Stack => usize::MAX,
        BundleType::Bin => ELF_HEADER_LEN,
        _ => 0,
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "corps interrompu",
            );
        };
        if kept.len() < keep {
            let take = (keep - kept.len()).min(chunk.len());
            kept.extend_from_slice(&chunk[..take]);
        }
        received += chunk.len() as u64;
        if expected.cut_after.is_some_and(|limit| received >= limit) {
            // Coupure simulée : la tâche de la connexion s'arrête, sans réponse et sans le message
            // qu'afficherait un vrai panic.
            std::panic::resume_unwind(Box::new("coupure simulée"));
        }
        hasher.update(&chunk);
        if let Some(verifier) = verifier.as_mut() {
            verifier.update(&chunk);
        }
    }
    if received != expected.length {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "le corps n'a pas la taille de Content-Length",
        );
    }
    let sha256 = hex::encode(hasher.finalize());
    if let Some(announced) = &expected.sha256
        && !announced.eq_ignore_ascii_case(&sha256)
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "artifact_invalid",
            "le SHA-256 annoncé n'est pas celui du fichier reçu",
        );
    }
    let mut signed_key_id = None;
    if let Some(verifier) = verifier {
        let Ok(verified) = verifier.finalize() else {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "signature_invalid",
                "signature invalide pour ce fichier",
            );
        };
        if verified.require_version(&expected.version).is_err() {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "signature_invalid",
                format!("la version signée n'est pas {}", expected.version),
            );
        }
        signed_key_id = Some(verified.key_id.to_hex());
    }
    if let Some((status, code, message)) = expected.reject {
        return error(StatusCode::from_u16(status).unwrap(), &code, message);
    }

    let mut state = lock(&state);
    if expected.bundle_type == BundleType::Bin
        && let Err(problem) = check_bin(&kept, expected.arch)
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "artifact_invalid",
            problem,
        );
    }
    let stack = if expected.bundle_type == BundleType::Stack {
        let Some(release) = state
            .apps
            .get(&expected.slug)
            .and_then(|app| app.releases.get(&expected.version.to_string()))
        else {
            return error(StatusCode::NOT_FOUND, "not_found", "version inconnue");
        };
        match inspect_stack(
            &state,
            &expected.slug,
            &expected.version,
            expected.arch,
            release,
            &kept,
        ) {
            Ok(found) => Some(found),
            Err(refusal) => return refusal.into_response(),
        }
    } else {
        None
    };
    let id = state.new_id();
    let base = state.url.clone();
    let Some(release) = state
        .apps
        .get_mut(&expected.slug)
        .and_then(|app| app.releases.get_mut(&expected.version.to_string()))
    else {
        return error(StatusCode::NOT_FOUND, "not_found", "version inconnue");
    };
    if let Some((images, keys)) = stack {
        // Les clés de stack.json deviennent celles de la version ; déjà déclarées, elles doivent
        // être les mêmes.
        let mut declared: Vec<String> = release
            .trusted_key_ids
            .iter()
            .map(|key| key.to_ascii_uppercase())
            .collect();
        let mut found: Vec<String> = keys.iter().map(|key| key.to_ascii_uppercase()).collect();
        declared.sort();
        found.sort();
        if !declared.is_empty() && declared != found {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "artifact_invalid",
                "les public_keys de stack.json ne sont pas les clés déclarées de la version",
            );
        }
        release.trusted_key_ids = keys;
        release.images = images;
    }
    let artifact = FakeArtifact {
        id,
        os: expected.os,
        arch: expected.arch,
        bundle_type: expected.bundle_type,
        filename: expected.filename,
        size: received,
        sha256,
        // Comme sur le serveur, la version signée ne vaut que pour un artefact signé.
        signed_version: signed_key_id.as_ref().map(|_| expected.version.to_string()),
        signed_key_id,
    };
    let reply = artifact_json(&artifact, &base);
    release.artifacts.retain(|existing| {
        (existing.os, existing.arch, existing.bundle_type)
            != (artifact.os, artifact.arch, artifact.bundle_type)
    });
    release.artifacts.push(artifact);
    (StatusCode::CREATED, Json(reply)).into_response()
}

/// Droits, emplacement, version, taille, nom et signature, avant la lecture du corps.
fn expect_upload(
    state: &mut State,
    headers: &HeaderMap,
    slug: String,
    version: &str,
    os: &str,
    arch: &str,
    bundle_type: &str,
) -> Result<Expected, Refusal> {
    authorize(state, headers, Some(WRITE_SCOPE), Some(&slug))?;
    let bad = |message: String| refuse(StatusCode::BAD_REQUEST, "invalid_request", message);
    let os: Os = os.parse().map_err(|problem| bad(format!("{problem}")))?;
    let arch: Arch = arch.parse().map_err(|problem| bad(format!("{problem}")))?;
    let bundle_type: BundleType = bundle_type
        .parse()
        .map_err(|problem| bad(format!("{problem}")))?;
    if bundle_type.os() != os || !arch.supported_on(os) {
        return Err(bad(format!(
            "emplacement {os}/{arch}/{bundle_type} impossible"
        )));
    }
    let Some(app) = state.apps.get(&slug) else {
        return Err(refuse(StatusCode::NOT_FOUND, "not_found", "app inconnue"));
    };
    if !kind_accepts(&app.kind, bundle_type) {
        return Err(bad(format!(
            "une app de type {} ne reçoit pas d'artefact {bundle_type}",
            app.kind
        )));
    }
    let version = parse_version(version)?;
    let Some(release) = app.releases.get(&version.to_string()) else {
        return Err(unknown_release(&version));
    };
    if release.status != "draft" {
        return Err(refuse(
            StatusCode::CONFLICT,
            "conflict",
            "la version n'est plus un brouillon",
        ));
    }
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let Some(length) =
        text(header::CONTENT_LENGTH.as_str()).and_then(|value| value.parse::<u64>().ok())
    else {
        return Err(refuse(
            StatusCode::LENGTH_REQUIRED,
            "invalid_request",
            "Content-Length obligatoire",
        ));
    };
    if length > state.max_artifact_bytes {
        return Err(refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "artefact trop gros : {} octets au plus",
                state.max_artifact_bytes
            ),
        ));
    }
    if let Some((status, code, message)) = state.refuse_next_upload.take() {
        return Err(refuse(
            StatusCode::from_u16(status).unwrap(),
            &code,
            message,
        ));
    }
    let Some(filename) = text(HEADER_FILENAME) else {
        return Err(bad("X-AppsMarket-Filename obligatoire".to_owned()));
    };
    let filename = am_protocol::sanitize_filename(&filename);
    let signature = match (text(HEADER_SIGNATURE), bundle_type) {
        (None, BundleType::Dmg) => None,
        (None, _) => return Err(bad("X-AppsMarket-Signature obligatoire".to_owned())),
        (Some(signature_text), _) => {
            let invalid = |message: String| {
                refuse(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "signature_invalid",
                    message,
                )
            };
            let signature = Signature::from_tauri(&signature_text)
                .map_err(|problem| invalid(format!("signature illisible : {problem}")))?;
            let key = app
                .keys
                .iter()
                .filter(|key| key.status == "active" || key.status == "next")
                .find(|key| key.key_id() == signature.key_id().to_hex())
                .ok_or_else(|| {
                    invalid(format!(
                        "la clé {} n'est ni active ni suivante pour cette app",
                        signature.key_id()
                    ))
                })?;
            let others = release
                .artifacts
                .iter()
                .filter(|artifact| {
                    (artifact.os, artifact.arch, artifact.bundle_type) != (os, arch, bundle_type)
                })
                .filter_map(|artifact| artifact.signed_key_id.as_deref());
            if let Some(other) = others
                .into_iter()
                .find(|other| *other != signature.key_id().to_hex())
            {
                return Err(invalid(format!(
                    "les artefacts de cette version sont signés par la clé {other}"
                )));
            }
            Some((signature, PublicKey::from_tauri(&key.public_key).unwrap()))
        }
    };
    let slot = format!("{os}/{arch}/{bundle_type}");
    let cut_after = match &state.cut_upload {
        Some((target, after)) if *target == slot => Some(*after),
        _ => None,
    };
    if cut_after.is_some() {
        state.cut_upload = None;
    }
    Ok(Expected {
        slug,
        version,
        os,
        arch,
        bundle_type,
        filename,
        length,
        signature,
        sha256: text(HEADER_SHA256),
        cut_after,
        reject: state.reject_next_upload.take(),
    })
}

/// Exécutable d'une app `cli`, contrôlé comme sur le serveur (`check_bin`) : un ELF d'une arch
/// connue, exécutable (`e_type` `ET_EXEC` ou `ET_DYN`), de l'arch de son emplacement.
fn check_bin(head: &[u8], declared: Arch) -> Result<(), String> {
    if !am_protocol::is_elf(head) {
        return Err("pas un exécutable Linux (ELF)".to_owned());
    }
    let found = am_protocol::elf_arch(head)
        .ok_or_else(|| "exécutable Linux pour une architecture inattendue".to_owned())?;
    if !am_protocol::elf_is_executable(head) {
        // `e_type`, aux octets 16 et 17, dans le boutisme que dit l'en-tête : le serveur le cite.
        let e_type = [head[16], head[17]];
        let e_type = match head[5] {
            2 => u16::from_be_bytes(e_type),
            _ => u16::from_le_bytes(e_type),
        };
        return Err(format!(
            "fichier ELF qui n'est pas un exécutable (type {e_type})"
        ));
    }
    if found == declared {
        Ok(())
    } else {
        Err(format!(
            "exécutable pour {found}, l'emplacement attend {declared}"
        ))
    }
}

/// Le type d'artefact que reçoit ce type d'app : `stack` pour une stack, `bin` pour une app cli,
/// les autres pour une app Swift ou Tauri.
fn kind_accepts(kind: &str, bundle_type: BundleType) -> bool {
    match kind {
        "stack" => bundle_type == BundleType::Stack,
        "cli" => bundle_type == BundleType::Bin,
        _ => !matches!(bundle_type, BundleType::Stack | BundleType::Bin),
    }
}

/// Contrôle une archive `stack` reçue, comme le serveur (docs/API.md, « Envoi d'une archive
/// stack ») ; rend ses images et les identifiants de ses clés.
fn inspect_stack(
    state: &State,
    slug: &str,
    version: &Version,
    arch: Arch,
    release: &FakeRelease,
    body: &[u8],
) -> Result<(BTreeMap<String, String>, Vec<String>), Refusal> {
    let invalid = |message: String| {
        refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "artifact_invalid",
            message,
        )
    };
    let archive = stack_archive::read(body)
        .map_err(|error| invalid(format!("archive : {error} ({})", error.code())))?;
    let manifest = StackManifest::parse_published(archive.stack_json(), slug, version)
        .map_err(|error| invalid(format!("{error} ({})", error.code())))?;
    let manifests = state.manifests.get(slug);
    let mut images = BTreeMap::new();
    for (name, digest) in &manifest.images {
        let available = manifests
            .and_then(|manifests| manifests.get(digest.as_str()))
            .is_some_and(|manifest| manifest.arch == Some(arch) || manifest.archs.contains(&arch));
        if !available {
            return Err(invalid(format!(
                "image {name} ({digest}) absente du serveur pour l'arch {arch}"
            )));
        }
        images.insert(name.clone(), digest.to_string());
    }
    let others_have_stacks = release
        .artifacts
        .iter()
        .any(|artifact| artifact.bundle_type == BundleType::Stack && artifact.arch != arch);
    if others_have_stacks && release.images != images {
        return Err(invalid(
            "les images de cette archive ne sont pas celles des autres archives de la version"
                .to_owned(),
        ));
    }
    let keys = manifest
        .trusted_key_ids()
        .iter()
        .map(|key| key.to_hex())
        .collect();
    Ok((images, keys))
}

/// Droits, app de type `stack` et empreinte, pour les routes des images.
fn expect_image(
    state: &State,
    headers: &HeaderMap,
    slug: &str,
    digest: &str,
) -> Result<StackDigest, Refusal> {
    authorize(state, headers, Some(WRITE_SCOPE), Some(slug))?;
    let Some(app) = state.apps.get(slug) else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("app {slug} inconnue"),
        ));
    };
    if app.kind != "stack" {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "seule une app de type stack reçoit des images",
        ));
    }
    StackDigest::parse(digest).map_err(|error| {
        refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    })
}

/// `HEAD …/blobs/{digest}` : 200 et la taille si la couche est là, 404 sinon.
async fn head_blob(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, digest)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let state = lock(&state);
    let digest = match expect_image(&state, &headers, &slug, &digest) {
        Ok(digest) => digest,
        Err(refusal) => return refusal.status.into_response(),
    };
    match state
        .blobs
        .get(&slug)
        .and_then(|blobs| blobs.get(digest.as_str()))
    {
        Some(blob) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, blob.len())
            .body(Body::empty())
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `PUT …/blobs/{digest}` : la couche, rangée si son empreinte est celle annoncée.
async fn put_blob(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, digest)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let checked = {
        let mut state = lock(&state);
        expect_image(&state, &headers, &slug, &digest).and_then(|digest| {
            let length = headers
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    refuse(
                        StatusCode::LENGTH_REQUIRED,
                        "invalid_request",
                        "Content-Length obligatoire",
                    )
                })?;
            if length > state.max_artifact_bytes {
                return Err(refuse(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    "couche trop grosse",
                ));
            }
            let cut_after = match &state.cut_upload {
                Some((target, after)) if target == "blob" => Some(*after),
                _ => None,
            };
            if cut_after.is_some() {
                state.cut_upload = None;
            }
            Ok((digest, length, cut_after))
        })
    };
    let (digest, length, cut_after) = match checked {
        Ok(checked) => checked,
        Err(refusal) => {
            drain(body.into_data_stream());
            return refusal.into_response();
        }
    };
    let mut data = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return invalid("corps interrompu");
        };
        data.extend_from_slice(&chunk);
        if cut_after.is_some_and(|limit| data.len() as u64 >= limit) {
            std::panic::resume_unwind(Box::new("coupure simulée"));
        }
    }
    if data.len() as u64 != length {
        return invalid("le corps n'a pas la taille de Content-Length");
    }
    let reply = json!({"digest": digest.as_str(), "size": length});
    let mut state = lock(&state);
    let blobs = state.blobs.entry(slug).or_default();
    if blobs.contains_key(digest.as_str()) {
        return (StatusCode::OK, Json(reply)).into_response();
    }
    if StackDigest::of(&data) != digest {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "artifact_invalid",
            "le contenu n'a pas l'empreinte annoncée",
        );
    }
    blobs.insert(digest.to_string(), data);
    (StatusCode::CREATED, Json(reply)).into_response()
}

async fn head_manifest(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, digest)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let state = lock(&state);
    let digest = match expect_image(&state, &headers, &slug, &digest) {
        Ok(digest) => digest,
        Err(refusal) => return refusal.status.into_response(),
    };
    match state
        .manifests
        .get(&slug)
        .and_then(|manifests| manifests.get(digest.as_str()))
    {
        Some(manifest) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, manifest.body.len())
            .header(header::CONTENT_TYPE, &manifest.media_type)
            .body(Body::empty())
            .unwrap(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn manifest_reply(digest: &str, manifest: &FakeManifest) -> Value {
    let mut reply = json!({"digest": digest, "media_type": manifest.media_type});
    if let Some(arch) = manifest.arch {
        reply["platform"] = json!(am_protocol::stack::docker_platform(arch));
    }
    if !manifest.archs.is_empty() {
        reply["platforms"] = json!(
            manifest
                .archs
                .iter()
                .filter_map(|arch| am_protocol::stack::docker_platform(*arch))
                .collect::<Vec<_>>()
        );
    }
    reply
}

/// `PUT …/manifests/{digest}` : un manifeste d'image ou un index, dont tout ce qu'il désigne est
/// déjà sur le serveur.
async fn put_manifest(
    Shared(state): Shared<SharedState>,
    UrlPath((slug, digest)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut state = lock(&state);
    let digest = match expect_image(&state, &headers, &slug, &digest) {
        Ok(digest) => digest,
        Err(refusal) => return refusal.into_response(),
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bare = content_type.split(';').next().unwrap_or_default().trim();
    if !oci::MANIFEST_TYPES.contains(&bare) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            format!("type de manifeste « {bare} » non pris en charge"),
        );
    }
    if body.len() > oci::MAX_MANIFEST_BYTES {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "manifeste de 2 Mio au plus",
        );
    }
    if let Some(existing) = state
        .manifests
        .get(&slug)
        .and_then(|manifests| manifests.get(digest.as_str()))
    {
        return (
            StatusCode::OK,
            Json(manifest_reply(digest.as_str(), existing)),
        )
            .into_response();
    }
    let invalid = |message: String| {
        error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "artifact_invalid",
            message,
        )
    };
    if StackDigest::of(&body) != digest {
        return invalid("le contenu n'a pas l'empreinte annoncée".to_owned());
    }
    let parsed = match oci::parse_manifest(&body, bare) {
        Ok(parsed) => parsed,
        Err(problem) => return invalid(format!("{problem} ({})", problem.code())),
    };
    let blobs = state.blobs.get(&slug);
    let manifests = state.manifests.get(&slug);
    let stored = match &parsed {
        OciObject::Image { config, layers } => {
            for descriptor in std::iter::once(config).chain(layers) {
                if !blobs.is_some_and(|blobs| blobs.contains_key(descriptor.digest.as_str())) {
                    return invalid(format!("{} absente du serveur", descriptor.digest));
                }
            }
            let config = &blobs.unwrap()[config.digest.as_str()];
            match oci::config_arch(config) {
                Ok(arch) => FakeManifest {
                    media_type: bare.to_owned(),
                    body: body.to_vec(),
                    arch: Some(arch),
                    archs: Vec::new(),
                },
                Err(problem) => return invalid(format!("{problem} ({})", problem.code())),
            }
        }
        OciObject::Index(entries) => {
            for entry in entries {
                let child =
                    manifests.and_then(|manifests| manifests.get(entry.descriptor.digest.as_str()));
                match child {
                    Some(child) if child.arch.is_some() => {}
                    Some(_) => {
                        return invalid(format!(
                            "{} n'est pas un manifeste d'image",
                            entry.descriptor.digest
                        ));
                    }
                    None => {
                        return invalid(format!("{} absent du serveur", entry.descriptor.digest));
                    }
                }
            }
            FakeManifest {
                media_type: bare.to_owned(),
                body: body.to_vec(),
                arch: None,
                archs: entries.iter().map(|entry| entry.arch).collect(),
            }
        }
    };
    let reply = manifest_reply(digest.as_str(), &stored);
    state
        .manifests
        .entry(slug)
        .or_default()
        .insert(digest.to_string(), stored);
    (StatusCode::CREATED, Json(reply)).into_response()
}

/// Les archs servies par une image de la stack, pour les essais : celles de son index.
pub fn served_archs(state: &State, app: &str, digest: &str) -> BTreeSet<Arch> {
    state
        .manifests
        .get(app)
        .and_then(|manifests| manifests.get(digest))
        .map(|manifest| {
            manifest
                .archs
                .iter()
                .copied()
                .chain(manifest.arch)
                .collect()
        })
        .unwrap_or_default()
}
