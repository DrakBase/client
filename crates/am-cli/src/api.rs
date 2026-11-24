//! Client de l'API de publication (docs/API.md, « Publication par l'outil » et « Envoi d'un
//! artefact ») : jeton `Authorization: Bearer amk_…`, corps JSON, artefacts envoyés en flux.
//!
//! Le client est asynchrone, dans un runtime tokio à un fil, derrière des méthodes bloquantes : le
//! client bloquant de reqwest perdrait la réponse d'un serveur qui refuse un envoi avant d'en avoir
//! lu tout le corps, et ne montrerait qu'une connexion coupée.

use std::error::Error as _;
use std::fmt;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

// Les objets de l'API de publication sont ceux du contrat, les mêmes que ceux du serveur.
pub use am_protocol::publish::{
    AppInfo, AppKind, Artifact, KeyStatus, NewRelease, Release, Screenshot, WhoAmI, WhoAmIToken,
};
use am_protocol::stack::Digest;
use am_protocol::update::{
    ApiError as ErrorBody, HEADER_FILENAME, HEADER_SHA256, HEADER_SIGNATURE,
};
use am_protocol::{Arch, BundleType, Os, Version};
use bytes::Bytes;
use futures_util::stream;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::credentials::{ServerUrl, Token};
use crate::home::AppSlug;

/// Délai d'une requête JSON. Un envoi d'artefact n'en a pas : il dure ce que dure le fichier.
const JSON_TIMEOUT: Duration = Duration::from_secs(60);
/// Morceaux lus dans le fichier pendant un envoi.
const CHUNK_BYTES: usize = 256 * 1024;
/// Intervalle minimal entre deux mises à jour de la progression.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

pub struct Api {
    server: ServerUrl,
    token: Token,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Api {
    pub fn new(server: ServerUrl, token: Token) -> anyhow::Result<Self> {
        // reqwest est compilé sans fournisseur cryptographique : rustls prend ring, que le serveur
        // emploie déjà. L'appel échoue seulement si un fournisseur est déjà installé.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("drakbase/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30));
        if server.is_loopback() {
            // Un serveur sur ce Mac ne passe jamais par un proxy, même avec HTTPS_PROXY défini.
            builder = builder.no_proxy();
        }
        let client = {
            let _runtime = runtime.enter();
            builder.build()?
        };
        Ok(Self {
            server,
            token,
            client,
            runtime,
        })
    }

    pub fn server(&self) -> &ServerUrl {
        &self.server
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, self.server.join(path))
            .bearer_auth(self.token.secret())
    }

    fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<Reply<T>, ApiError> {
        self.runtime.block_on(async {
            let response = request
                .timeout(JSON_TIMEOUT)
                .send()
                .await
                .map_err(ApiError::transport)?;
            read_reply(response).await
        })
    }

    pub fn whoami(&self) -> Result<WhoAmI, ApiError> {
        let request = self.request(Method::GET, "/api/v1/publish/whoami");
        Ok(self.json(request)?.body)
    }

    pub fn app(&self, app: &AppSlug) -> Result<AppInfo, ApiError> {
        let request = self.request(Method::GET, &format!("/api/v1/publish/apps/{app}"));
        Ok(self.json(request)?.body)
    }

    /// Crée le brouillon, ou rend celui qui existe déjà : `true` s'il vient d'être créé.
    pub fn create_release(
        &self,
        app: &AppSlug,
        release: &NewRelease,
    ) -> Result<(Release, bool), ApiError> {
        let request = self
            .request(
                Method::POST,
                &format!("/api/v1/publish/apps/{app}/releases"),
            )
            .json(release);
        let reply = self.json(request)?;
        Ok((reply.body, reply.status == StatusCode::CREATED))
    }

    /// Retire un artefact d'un brouillon : le serveur refuse (409) celui d'une version publiée.
    pub fn delete_artifact(&self, app: &AppSlug, artifact_id: &str) -> Result<(), ApiError> {
        let request = self.request(
            Method::DELETE,
            &format!("/api/v1/publish/apps/{app}/artifacts/{artifact_id}"),
        );
        self.runtime.block_on(async {
            let response = request
                .timeout(JSON_TIMEOUT)
                .send()
                .await
                .map_err(ApiError::transport)?;
            success_body(response).await.map(|_| ())
        })
    }

    pub fn publish(&self, app: &AppSlug, version: &Version) -> Result<Release, ApiError> {
        // Un corps JSON vide, au cas où le serveur exige du JSON sur toute requête POST.
        let request = self
            .request(
                Method::POST,
                &format!("/api/v1/publish/apps/{app}/releases/{version}/publish"),
            )
            .json(&serde_json::json!({}));
        Ok(self.json(request)?.body)
    }

    /// Envoie un artefact en flux, avec sa progression au terminal.
    pub fn upload(
        &self,
        app: &AppSlug,
        version: &Version,
        upload: &Upload<'_>,
    ) -> Result<Artifact, ApiError> {
        let file = File::open(upload.path).map_err(|error| {
            ApiError::Local(format!("lecture de {} : {error}", upload.path.display()))
        })?;
        let progress = Arc::new(Mutex::new(Progress::new(
            format!("{}/{}/{}", upload.os, upload.arch, upload.bundle_type),
            upload.size,
        )));
        let mut request = self
            .request(
                Method::PUT,
                &format!(
                    "/api/v1/publish/apps/{app}/releases/{version}/artifacts/{}/{}/{}",
                    upload.os, upload.arch, upload.bundle_type
                ),
            )
            // Le corps est un flux : sans cet en-tête, il partirait par morceaux (chunked).
            .header(CONTENT_LENGTH, upload.size)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(HEADER_FILENAME, upload.file_name)
            .header(HEADER_SHA256, upload.sha256)
            .body(reqwest::Body::wrap_stream(file_chunks(
                file,
                upload.size,
                Arc::clone(&progress),
            )));
        if let Some(signature) = upload.signature {
            request = request.header(HEADER_SIGNATURE, signature);
        }
        let result = self.runtime.block_on(async {
            let response = request.send().await.map_err(ApiError::transport)?;
            read_reply(response).await
        });
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish(result.is_ok());
        result.map(|reply| reply.body)
    }

    /// Taille d'une couche (ou d'une configuration) d'image que le serveur a déjà ; `None` s'il
    /// ne l'a pas (docs/API.md, « Stacks Docker », publication des images).
    pub fn blob_size(&self, app: &AppSlug, digest: &Digest) -> Result<Option<u64>, ApiError> {
        let request = self.request(
            Method::HEAD,
            &format!("/api/v1/publish/apps/{app}/blobs/{digest}"),
        );
        self.head(request)
    }

    /// Vrai si le serveur a déjà ce manifeste ou cet index.
    pub fn has_manifest(&self, app: &AppSlug, digest: &Digest) -> Result<bool, ApiError> {
        let request = self.request(
            Method::HEAD,
            &format!("/api/v1/publish/apps/{app}/manifests/{digest}"),
        );
        Ok(self.head(request)?.is_some())
    }

    /// `HEAD` : la taille annoncée si l'objet existe (200), `None` pour 404. Une réponse `HEAD`
    /// n'a pas de corps : les refus courants reçoivent ici leur code du contrat.
    fn head(&self, request: RequestBuilder) -> Result<Option<u64>, ApiError> {
        self.runtime.block_on(async {
            let response = request
                .timeout(JSON_TIMEOUT)
                .send()
                .await
                .map_err(ApiError::transport)?;
            let status = response.status();
            let size = response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let refused = |code: &str, message: &str| ApiError::Server {
                code: code.to_owned(),
                message: message.to_owned(),
            };
            match status {
                StatusCode::OK => Ok(Some(size.unwrap_or_default())),
                StatusCode::NOT_FOUND => Ok(None),
                StatusCode::UNAUTHORIZED => {
                    Err(refused("unauthorized", "jeton absent, inconnu ou expiré"))
                }
                StatusCode::FORBIDDEN => Err(refused("forbidden", "accès refusé")),
                StatusCode::BAD_REQUEST => Err(refused(
                    "invalid_request",
                    "requête refusée : seule une app de type stack reçoit des images",
                )),
                status if status.is_redirection() => Err(ApiError::Redirect {
                    status: status.as_u16(),
                    location: response
                        .headers()
                        .get(LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("?")
                        .to_owned(),
                }),
                status => Err(ApiError::Http {
                    status: status.as_u16(),
                }),
            }
        })
    }

    /// Envoie une couche ou une configuration d'image, en flux : `true` si le serveur vient de la
    /// ranger, `false` s'il l'avait déjà.
    pub fn put_blob(
        &self,
        app: &AppSlug,
        digest: &Digest,
        path: &Path,
        size: u64,
        label: &str,
    ) -> Result<bool, ApiError> {
        let file = File::open(path)
            .map_err(|error| ApiError::Local(format!("lecture de {} : {error}", path.display())))?;
        let progress = Arc::new(Mutex::new(Progress::new(label.to_owned(), size)));
        let request = self
            .request(
                Method::PUT,
                &format!("/api/v1/publish/apps/{app}/blobs/{digest}"),
            )
            .header(CONTENT_LENGTH, size)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(reqwest::Body::wrap_stream(file_chunks(
                file,
                size,
                Arc::clone(&progress),
            )));
        let result = self.runtime.block_on(async {
            let response = request.send().await.map_err(ApiError::transport)?;
            read_reply::<StoredBlob>(response).await
        });
        progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finish(result.is_ok());
        let reply = result?;
        if reply.body.digest != *digest || reply.body.size != size {
            return Err(ApiError::Decode(format!(
                "le serveur a rangé {} ({} octets) au lieu de {digest} ({size} octets)",
                reply.body.digest, reply.body.size
            )));
        }
        Ok(reply.status == StatusCode::CREATED)
    }

    /// Envoie un manifeste d'image ou un index, avec son type : `true` si le serveur vient de le
    /// ranger, `false` s'il l'avait déjà.
    pub fn put_manifest(
        &self,
        app: &AppSlug,
        digest: &Digest,
        media_type: &str,
        body: &[u8],
    ) -> Result<bool, ApiError> {
        let request = self
            .request(
                Method::PUT,
                &format!("/api/v1/publish/apps/{app}/manifests/{digest}"),
            )
            .header(CONTENT_TYPE, media_type)
            .body(body.to_vec());
        let reply = self.runtime.block_on(async {
            let response = request
                .timeout(JSON_TIMEOUT)
                .send()
                .await
                .map_err(ApiError::transport)?;
            read_reply::<StoredManifest>(response).await
        })?;
        if reply.body.digest != *digest {
            return Err(ApiError::Decode(format!(
                "le serveur a rangé {} au lieu de {digest}",
                reply.body.digest
            )));
        }
        Ok(reply.status == StatusCode::CREATED)
    }

    /// Change la fiche publique de l'app (portée `listing:write`) ; rend l'app telle que `app`.
    pub fn update_app(&self, app: &AppSlug, changes: &AppChanges) -> Result<AppInfo, ApiError> {
        let request = self
            .request(Method::PATCH, &format!("/api/v1/publish/apps/{app}"))
            .json(changes);
        Ok(self.json(request)?.body)
    }

    /// Ajoute une capture d'écran à la fiche : `true` si elle vient d'être créée, `false` si l'app
    /// avait déjà cette image (même SHA-256), que le serveur rend alors.
    pub fn add_screenshot(
        &self,
        app: &AppSlug,
        png: &[u8],
    ) -> Result<(Screenshot, bool), ApiError> {
        let request = self
            .request(
                Method::POST,
                &format!("/api/v1/publish/apps/{app}/screenshots"),
            )
            .header(CONTENT_TYPE, "image/png")
            .body(png.to_vec());
        // Pas de délai total : 8 Mio sur une connexion lente prennent plus d'une minute.
        let reply = self.runtime.block_on(async {
            let response = request.send().await.map_err(ApiError::transport)?;
            read_reply::<Screenshot>(response).await
        })?;
        Ok((reply.body, reply.status == StatusCode::CREATED))
    }

    pub fn set_screenshot_caption(
        &self,
        app: &AppSlug,
        id: &str,
        caption: &str,
    ) -> Result<Screenshot, ApiError> {
        let request = self
            .request(
                Method::PATCH,
                &format!("/api/v1/publish/apps/{app}/screenshots/{id}"),
            )
            .json(&serde_json::json!({ "caption": caption }));
        Ok(self.json(request)?.body)
    }

    /// Donne l'ordre complet des captures : chacune une fois.
    pub fn order_screenshots(
        &self,
        app: &AppSlug,
        ids: &[&str],
    ) -> Result<Vec<Screenshot>, ApiError> {
        let request = self
            .request(
                Method::PUT,
                &format!("/api/v1/publish/apps/{app}/screenshots/order"),
            )
            .json(&serde_json::json!({ "ids": ids }));
        Ok(self.json(request)?.body)
    }

    pub fn delete_screenshot(&self, app: &AppSlug, id: &str) -> Result<(), ApiError> {
        let request = self.request(
            Method::DELETE,
            &format!("/api/v1/publish/apps/{app}/screenshots/{id}"),
        );
        self.runtime.block_on(async {
            let response = request
                .timeout(JSON_TIMEOUT)
                .send()
                .await
                .map_err(ApiError::transport)?;
            success_body(response).await.map(|_| ())
        })
    }
}

/// Le fichier, morceau par morceau, en s'arrêtant à la taille annoncée dans `Content-Length`.
fn file_chunks(
    file: File,
    size: u64,
    progress: Arc<Mutex<Progress>>,
) -> impl futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static {
    // Lectures bloquantes, mais sur le disque local : le runtime n'a que cet envoi à faire.
    stream::unfold(Some((file.take(size), 0u64)), move |state| {
        let progress = Arc::clone(&progress);
        async move {
            let (mut file, sent) = state?;
            let mut chunk = vec![0; CHUNK_BYTES];
            match file.read(&mut chunk) {
                Ok(0) if sent < size => Some((
                    Err(io::Error::other(
                        "le fichier a raccourci pendant l'envoi : relance la commande",
                    )),
                    None,
                )),
                Ok(0) => None,
                Ok(read) => {
                    chunk.truncate(read);
                    let sent = sent + read as u64;
                    progress
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .show(sent);
                    Some((Ok(Bytes::from(chunk)), Some((file, sent))))
                }
                Err(error) => Some((Err(error), None)),
            }
        }
    })
}

/// Un artefact à envoyer.
pub struct Upload<'a> {
    pub os: Os,
    pub arch: Arch,
    pub bundle_type: BundleType,
    pub path: &'a Path,
    pub size: u64,
    /// Nom déjà réduit à `[A-Za-z0-9._-]`.
    pub file_name: &'a str,
    /// Contenu du `.sig`, sans blanc autour ; absent pour un dmg.
    pub signature: Option<&'a str>,
    /// SHA-256 en hexadécimal minuscule.
    pub sha256: &'a str,
}

/// Progression d'un envoi, sur une ligne réécrite de la sortie d'erreur, seulement au terminal.
struct Progress {
    label: String,
    total: u64,
    terminal: bool,
    shown: Option<Instant>,
}

impl Progress {
    fn new(label: String, total: u64) -> Self {
        Self {
            label,
            total,
            terminal: io::stderr().is_terminal(),
            shown: None,
        }
    }

    fn show(&mut self, sent: u64) {
        if !self.terminal
            || self
                .shown
                .is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL && sent < self.total)
        {
            return;
        }
        let percent = (sent * 100).checked_div(self.total).unwrap_or(100);
        let mut stderr = io::stderr().lock();
        let _ = write!(
            stderr,
            "\r\x1b[2K  {} : {percent} % ({} sur {})",
            self.label,
            am_protocol::human_size(sent),
            am_protocol::human_size(self.total)
        );
        let _ = stderr.flush();
        self.shown = Some(Instant::now());
    }

    /// Efface la ligne après un envoi réussi ; la garde, terminée, après un échec.
    fn finish(&mut self, success: bool) {
        if self.terminal && self.shown.is_some() {
            if success {
                eprint!("\r\x1b[2K");
            } else {
                eprintln!();
            }
        }
        self.terminal = false;
    }
}

struct Reply<T> {
    status: StatusCode,
    body: T,
}

async fn read_reply<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<Reply<T>, ApiError> {
    let (status, bytes) = success_body(response).await?;
    Ok(Reply {
        status,
        body: decode(&bytes)?,
    })
}

/// Lit le corps JSON d'une réponse réussie. Chaque champ du contrat est attendu (docs/API.md) :
/// s'il en manque un, l'erreur le nomme, plutôt que de laisser l'outil deviner à sa place.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|error| {
        let text = error.to_string();
        // serde le dit ainsi : « missing field `kind` at line 1 column 42 ».
        let missing = text
            .strip_prefix("missing field `")
            .and_then(|rest| rest.split_once('`'));
        ApiError::Decode(match missing {
            Some((field, _)) => format!(
                "il y manque le champ « {field} » : le serveur est sans doute plus ancien que cet outil (drakbase {})",
                env!("CARGO_PKG_VERSION")
            ),
            None => text,
        })
    })
}

/// Statut et corps d'une réponse réussie ; sinon l'erreur du contrat, une redirection ou le
/// statut seul.
async fn success_body(response: reqwest::Response) -> Result<(StatusCode, Bytes), ApiError> {
    let status = response.status();
    if status.is_redirection() {
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("?")
            .to_owned();
        return Err(ApiError::Redirect {
            status: status.as_u16(),
            location,
        });
    }
    let bytes = response.bytes().await.map_err(ApiError::transport);
    if status.is_success() {
        return Ok((status, bytes?));
    }
    match bytes
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ErrorBody>(&bytes).ok())
    {
        Some(body) => Err(ApiError::Server {
            code: body.error,
            message: body.message,
        }),
        None => Err(ApiError::Http {
            status: status.as_u16(),
        }),
    }
}

/// Échec d'une requête.
#[derive(Debug)]
pub enum ApiError {
    /// Réponse d'erreur du contrat : `{"error", "message"}`.
    Server {
        code: String,
        message: String,
    },
    /// Statut d'erreur sans ce corps : un proxy, une page HTML…
    Http {
        status: u16,
    },
    Redirect {
        status: u16,
        location: String,
    },
    /// Réponse réussie, mais illisible.
    Decode(String),
    /// Le serveur n'a pas pu répondre : connexion, délai, certificat…
    Transport(String),
    /// Le fichier à envoyer n'a pas pu être lu.
    Local(String),
}

impl ApiError {
    /// Le code d'erreur du contrat, s'il y en a un.
    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    fn transport(error: reqwest::Error) -> Self {
        ApiError::Transport(describe_transport(&error))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Server { code, message } => write!(f, "{message} ({code})"),
            ApiError::Http { status } => {
                match StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                {
                    Some(reason) => write!(f, "le serveur répond HTTP {status} {reason}"),
                    None => write!(f, "le serveur répond HTTP {status}"),
                }
            }
            ApiError::Redirect { status, location } => write!(
                f,
                "le serveur redirige vers {location} (HTTP {status}) : vérifie l'adresse donnée à « drakbase login »"
            ),
            ApiError::Decode(detail) => write!(f, "réponse du serveur illisible : {detail}"),
            ApiError::Transport(detail) | ApiError::Local(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for ApiError {}

/// Une erreur réseau en français, avec sa cause la plus précise.
fn describe_transport(error: &reqwest::Error) -> String {
    let what = if error.is_timeout() {
        "délai dépassé"
    } else if error.is_connect() {
        "connexion impossible"
    } else if error.is_body() {
        "envoi interrompu"
    } else {
        "échec de la requête"
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = error.source();
    let mut detail = None;
    while let Some(current) = cause {
        if let Some(tls) = current.downcast_ref::<rustls::Error>() {
            detail = Some(describe_tls(tls));
            break;
        }
        if let Some(io) = current.downcast_ref::<io::Error>() {
            if let Some(tls) = tls_error(io) {
                detail = Some(describe_tls(tls));
                break;
            }
            detail = Some(match io.kind() {
                io::ErrorKind::ConnectionRefused => "connexion refusée par le serveur".to_owned(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => {
                    "connexion coupée par le serveur".to_owned()
                }
                io::ErrorKind::TimedOut => "délai dépassé".to_owned(),
                io::ErrorKind::UnexpectedEof => "connexion fermée avant la réponse".to_owned(),
                _ => io.to_string(),
            });
        }
        if detail.is_none() && current.source().is_none() {
            detail = Some(current.to_string());
        }
        cause = current.source();
    }
    match detail {
        Some(detail) if detail != what => format!("{what} : {detail}"),
        _ => what.to_owned(),
    }
}

/// L'erreur de rustls, que tokio-rustls et hyper enveloppent dans des io::Error, dont `source()`
/// saute le contenu.
fn tls_error(io: &io::Error) -> Option<&rustls::Error> {
    let inner = io.get_ref()?;
    inner
        .downcast_ref::<rustls::Error>()
        .or_else(|| inner.downcast_ref::<io::Error>().and_then(tls_error))
}

fn describe_tls(error: &rustls::Error) -> String {
    use rustls::CertificateError as Certificate;
    let rustls::Error::InvalidCertificate(certificate) = error else {
        return format!("connexion sécurisée impossible : {error}");
    };
    let reason = match certificate {
        Certificate::Expired | Certificate::ExpiredContext { .. } => "il a expiré".to_owned(),
        Certificate::NotValidYet | Certificate::NotValidYetContext { .. } => {
            "il n'est pas encore valable".to_owned()
        }
        Certificate::NotValidForName | Certificate::NotValidForNameContext { .. } => {
            "il est fait pour un autre nom de serveur".to_owned()
        }
        Certificate::UnknownIssuer => "son émetteur est inconnu de ce Mac".to_owned(),
        Certificate::Revoked => "il a été révoqué".to_owned(),
        // Le vérificateur du système donne alors le motif de macOS : non approuvé, expiré…
        Certificate::Other(other) => other.to_string(),
        other => format!("{other:?}"),
    };
    format!("certificat du serveur refusé : {reason}")
}

/// Changements de la fiche publique d'une app : le corps de `PATCH …/apps/{app}`.
#[derive(Default, Serialize)]
pub struct AppChanges {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// « une app Swift », « une stack Docker »…, pour les messages.
pub const fn described(kind: AppKind) -> &'static str {
    match kind {
        AppKind::Swift => "une app Swift",
        AppKind::Tauri => "une app Tauri",
        AppKind::Stack => "une stack Docker",
        AppKind::Cli => "une app cli (exécutable Linux)",
    }
}

/// L'état d'une clé, pour les messages.
pub const fn key_status_label(status: KeyStatus) -> &'static str {
    match status {
        KeyStatus::Active => "active",
        KeyStatus::Next => "suivante",
        KeyStatus::Retired => "retirée",
    }
}

/// Réponse à l'envoi d'une couche : `{digest, size}`.
#[derive(Debug, Deserialize)]
struct StoredBlob {
    digest: Digest,
    size: u64,
}

/// Réponse à l'envoi d'un manifeste : `{digest, media_type, platform?, platforms?}`.
#[derive(Debug, Deserialize)]
struct StoredManifest {
    digest: Digest,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'app que rend `GET …/apps/{app}`.
    fn app_info() -> serde_json::Value {
        serde_json::json!({
            "slug": "demo", "name": "Démo", "kind": "swift", "bundle_id": null,
            "description": "", "screenshots": [], "keys": [],
            "latest": {"stable": "1.2.0", "beta": null},
        })
    }

    /// Un champ du contrat qui manque n'est pas remplacé par une valeur par défaut : la réponse
    /// est refusée, et l'erreur le nomme.
    #[test]
    fn a_missing_field_is_named_instead_of_guessed() {
        let without = |field: &str| {
            let mut whoami = serde_json::json!({
                "user": {"email": "a@example.fr", "name": "A", "role": "editor"},
                "token": {"name": "mac", "scopes": [], "app": null,
                    "expires_at": "2027-10-25T00:00:00Z"},
                "max_artifact_bytes": 1073741824, "public_url": "https://maj.example.fr",
            });
            whoami.as_object_mut().unwrap().remove(field);
            decode::<WhoAmI>(whoami.to_string().as_bytes())
                .unwrap_err()
                .to_string()
        };
        for field in ["max_artifact_bytes", "public_url"] {
            let error = without(field);
            assert!(
                error.contains(&format!(
                    "réponse du serveur illisible : il y manque le champ « {field} »"
                )),
                "{error}"
            );
            assert!(error.contains("plus ancien que cet outil"), "{error}");
        }
        for field in ["kind", "description", "screenshots", "keys", "latest"] {
            let mut app = app_info();
            app.as_object_mut().unwrap().remove(field);
            let error = decode::<AppInfo>(app.to_string().as_bytes())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("il y manque le champ « {field} »")),
                "{error}"
            );
        }
        // Une réponse complète se lit ; un artefact sans identifiant ne pourrait pas être retiré
        // d'un brouillon.
        assert!(decode::<AppInfo>(app_info().to_string().as_bytes()).is_ok());
        let error = decode::<Artifact>(
            br#"{"os": "darwin", "arch": "aarch64", "bundle_type": "app", "filename": "a",
                "size": 1, "sha256": "ab", "url": "https://maj.example.fr/dl/x",
                "created_at": "2026-10-25T12:00:00Z"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("il y manque le champ « id »"), "{error}");
        // Une autre erreur de lecture garde le texte de serde.
        let error = decode::<WhoAmI>(b"<html>").unwrap_err().to_string();
        assert!(
            error.starts_with("réponse du serveur illisible : "),
            "{error}"
        );
        assert!(!error.contains("il y manque"), "{error}");
    }

    /// Le faux serveur des essais, avec `test-app` de ce type, et le client du jeton complet.
    fn fake_server(kind: &str) -> (crate::fake_server::FakeServer, Api, AppSlug) {
        let mut state = crate::fake_server::State::demo(None);
        state.apps.get_mut("test-app").unwrap().kind = kind.to_owned();
        let server = crate::fake_server::FakeServer::start(state);
        let api = Api::new(
            ServerUrl::parse(&server.url).unwrap(),
            Token::parse("amk_essai_complet").unwrap(),
        )
        .unwrap();
        (server, api, "test-app".parse().unwrap())
    }

    /// Le serveur lit la version d'un brouillon en forme souple et la rend canonique ; il met en
    /// majuscules les identifiants des clés embarquées, sans doublon.
    #[test]
    fn a_draft_takes_a_loose_version_and_loose_key_ids() {
        let (server, api, app) = fake_server("swift");
        let draft = |version: &str, keys: &[&str]| NewRelease {
            version: version.to_owned(),
            trusted_key_ids: Some(keys.iter().map(|key| (*key).to_owned()).collect()),
            ..NewRelease::default()
        };
        let keys = ["0123456789abcdef", " 0123456789ABCDEF ", "FEDCBA9876543210"];
        let (release, created) = api.create_release(&app, &draft("v1.2", &keys)).unwrap();
        assert!(created);
        assert_eq!(release.version.to_string(), "1.2.0");
        assert_eq!(
            release.trusted_key_ids,
            ["0123456789ABCDEF", "FEDCBA9876543210"]
        );
        // La même version, écrite autrement : le même brouillon, qui prend les clés envoyées.
        let (again, created) = api
            .create_release(&app, &draft("1.2.0", &["fedcba9876543210"]))
            .unwrap();
        assert!(!created);
        assert_eq!(again.trusted_key_ids, ["FEDCBA9876543210"]);
        assert_eq!(
            server.state().apps["test-app"]
                .releases
                .keys()
                .collect::<Vec<_>>(),
            ["1.2.0"]
        );

        let error = api
            .create_release(&app, &draft("pas une version", &[]))
            .unwrap_err();
        assert_eq!(error.code(), Some("version_invalid"), "{error}");
        let error = api
            .create_release(&app, &draft("1.3.0", &["0123"]))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "identifiant de clé « 0123 » : 16 caractères hexadécimaux attendus (invalid_request)"
        );
        // La version du chemin se lit de même, à la publication : sans artefact, un refus.
        let error = api
            .publish(&app, &Version::parse_release("1.2").unwrap())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("aucun artefact de mise à jour"),
            "{error}"
        );
    }

    /// Un exécutable d'une app cli est contrôlé par le serveur une fois reçu : un fichier ELF qui
    /// n'est pas un exécutable, un fichier objet par exemple, est refusé.
    #[test]
    fn the_server_refuses_an_elf_that_is_not_an_executable() {
        use sha2::{Digest as _, Sha256};

        let (server, api, app) = fake_server("cli");
        let version = Version::parse_release("0.4.0").unwrap();
        let draft = NewRelease {
            version: version.to_string(),
            ..NewRelease::default()
        };
        api.create_release(&app, &draft).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let secret = std::fs::read_to_string(fixtures.join("keys/test-app.secret")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Un en-tête ELF 64 bits, petit-boutien, pour arm64, de ce type (`e_type`).
        let send = |kind: u8| {
            let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
            bytes.resize(64, 0);
            bytes[16] = kind;
            bytes[18] = 0xb7;
            let path = dir.path().join("agent");
            std::fs::write(&path, &bytes).unwrap();
            let signature = am_protocol::signing::sign(
                secret.trim(),
                "appsmarket-fixtures",
                bytes.as_slice(),
                "agent",
                &version,
                0,
            )
            .unwrap();
            api.upload(
                &app,
                &version,
                &Upload {
                    os: Os::Linux,
                    arch: Arch::Aarch64,
                    bundle_type: BundleType::Bin,
                    path: &path,
                    size: bytes.len() as u64,
                    file_name: "agent",
                    signature: Some(&signature),
                    sha256: &hex::encode(Sha256::digest(&bytes)),
                },
            )
        };
        let error = send(1).unwrap_err();
        assert_eq!(
            error.to_string(),
            "fichier ELF qui n'est pas un exécutable (type 1) (artifact_invalid)"
        );
        assert!(
            server.state().apps["test-app"].releases["0.4.0"]
                .artifacts
                .is_empty()
        );
        // Un exécutable passe, et le serveur garde la clé et la version de sa signature.
        let sent = send(2).unwrap();
        assert!(sent.signed_key_id.is_some());
        assert_eq!(sent.signed_version.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn the_listing_change_carries_the_description_only() {
        assert_eq!(
            serde_json::to_value(AppChanges {
                description: Some("Une app.".to_owned())
            })
            .unwrap(),
            serde_json::json!({"description": "Une app."})
        );
        assert_eq!(
            serde_json::to_value(AppChanges::default()).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn errors_read_like_the_contract() {
        let server = ApiError::Server {
            code: "signature_invalid".to_owned(),
            message: "signature refusée".to_owned(),
        };
        assert_eq!(server.to_string(), "signature refusée (signature_invalid)");
        assert_eq!(server.code(), Some("signature_invalid"));
        assert_eq!(
            ApiError::Http { status: 502 }.to_string(),
            "le serveur répond HTTP 502 Bad Gateway"
        );
        assert_eq!(ApiError::Http { status: 502 }.code(), None);
    }
}
