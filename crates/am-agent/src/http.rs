//! Requêtes au serveur : vérification (docs/PROTOCOLE.md, « Vérification ») et téléchargement
//! d'un artefact, avec contrôle de sa taille et de son SHA-256.
//!
//! Le client est asynchrone, dans un runtime tokio à un fil, derrière des méthodes bloquantes,
//! comme celui de l'outil `drakbase`. rustls y prend le fournisseur ring, qui se compile en
//! statique pour Linux musl.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use am_protocol::update::{ApiError, HEADER_INSTALL_ID, UpdateResponse};
use am_protocol::{Arch, BundleType, Channel, ServerUrl, Version};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};

use crate::files;

/// Délai de la vérification.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Délai sans aucun octet reçu pendant un téléchargement.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Taille maximale de la réponse à une vérification : quelques champs et des notes.
const MAX_CHECK_BYTES: usize = 1024 * 1024;

pub struct Http {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

/// Une vérification : `GET /api/v1/update/{app}/linux/{arch}/{version}?bundle_type=…`.
pub struct Check<'a> {
    pub server: &'a ServerUrl,
    pub app: &'a str,
    pub arch: Arch,
    pub version: &'a Version,
    pub bundle_type: BundleType,
    pub channel: Channel,
    /// Version de Docker Compose, pour une stack.
    pub os_version: Option<&'a str>,
    pub install_id: Option<&'a str>,
}

impl Http {
    pub fn new() -> Result<Self> {
        // reqwest est compilé sans fournisseur cryptographique : rustls prend ring. L'appel
        // échoue seulement si un fournisseur est déjà installé.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let builder = reqwest::Client::builder()
            .user_agent(concat!("drakbase-stack/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            // L'agent ne va chercher un fichier que là où le serveur de la stack le sert : la
            // seule redirection suivie mène à la même adresse, en https.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let stays = attempt.url().scheme() == "https"
                    && attempt
                        .previous()
                        .last()
                        .is_some_and(|from| same_server(from, attempt.url()));
                if stays && attempt.previous().len() <= 2 {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .read_timeout(READ_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(30));
        let client = {
            let _runtime = runtime.enter();
            builder.build()?
        };
        Ok(Self { client, runtime })
    }

    /// Rend la mise à jour proposée, ou `None` si tout est à jour (204).
    pub fn check(&self, check: &Check<'_>) -> Result<Option<UpdateResponse>> {
        let mut url = reqwest::Url::parse(&check.server.join(&format!(
            "/api/v1/update/{}/linux/{}/",
            check.app, check.arch
        )))
        .context("adresse de vérification")?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("adresse de vérification"))?
            .pop_if_empty()
            .push(&check.version.to_string());
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("bundle_type", check.bundle_type.as_str());
            query.append_pair("channel", check.channel.as_str());
            if let Some(os_version) = check.os_version {
                query.append_pair("os_version", os_version);
            }
        }
        let mut request = self.client.get(url).timeout(CHECK_TIMEOUT);
        if let Some(id) = check.install_id {
            request = request.header(HEADER_INSTALL_ID, id);
        }
        let server = check.server;
        self.runtime.block_on(async {
            let response = request
                .send()
                .await
                .map_err(|error| transport(server, error))?;
            let status = response.status();
            match status {
                StatusCode::NO_CONTENT => Ok(None),
                StatusCode::OK => {
                    let body = limited_body(server, response).await?;
                    let update: UpdateResponse = serde_json::from_slice(&body)
                        .context("réponse de vérification du serveur illisible")?;
                    Ok(Some(update))
                }
                StatusCode::NOT_FOUND => Err(anyhow!(
                    "le serveur {server} ne connaît pas l'app « {} » (inconnue ou archivée)",
                    check.app
                )),
                _ => {
                    let body = limited_body(server, response).await.unwrap_or_default();
                    Err(api_error(server, status, &body))
                }
            }
        })
    }

    /// Télécharge `url` dans `dest`, en contrôlant la taille et le SHA-256 annoncés. `url` est une
    /// adresse de `server` (même schéma, même hôte, même port) : la réponse d'une vérification ne
    /// peut pas envoyer l'agent ailleurs. `dest` est toujours créé, jamais rouvert : ce qui se
    /// trouve à sa place est retiré, et un lien posé là n'est pas suivi. Il est retiré si le
    /// téléchargement échoue.
    pub fn download(
        &self,
        server: &ServerUrl,
        url: &str,
        dest: &Path,
        size: u64,
        sha256: &str,
    ) -> Result<()> {
        let result = self.download_inner(server, url, dest, size, sha256);
        if result.is_err() {
            let _ = fs::remove_file(dest);
        }
        result
    }

    fn download_inner(
        &self,
        server: &ServerUrl,
        url: &str,
        dest: &Path,
        size: u64,
        sha256: &str,
    ) -> Result<()> {
        let parsed = reqwest::Url::parse(url)
            .with_context(|| format!("adresse de téléchargement « {url} » illisible"))?;
        if !same_origin(server, &parsed) {
            bail!(
                "adresse de téléchargement « {url} » : ce n'est pas une adresse du serveur {server}. Si ce serveur est connu sous cette autre adresse, la stack doit être installée avec elle (--server)"
            );
        }
        let mut file = files::remove_entry(dest)
            .and_then(|()| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(dest)
            })
            .with_context(|| format!("écriture de {}", dest.display()))?;
        let request = self.client.get(parsed);
        let digest = self.runtime.block_on(async {
            let mut response = request
                .send()
                .await
                .with_context(|| format!("téléchargement de {url}"))?;
            match response.status() {
                StatusCode::OK => {}
                StatusCode::GONE => {
                    bail!("cette version a été retirée du serveur pendant la mise à jour (410)")
                }
                status => bail!("téléchargement de {url} : réponse {status}"),
            }
            let mut hasher = Sha256::new();
            let mut received: u64 = 0;
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("téléchargement de {url} interrompu"))?
            {
                received += chunk.len() as u64;
                if received > size {
                    bail!("le fichier téléchargé dépasse la taille annoncée ({size} octets)");
                }
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .with_context(|| format!("écriture de {}", dest.display()))?;
            }
            if received != size {
                bail!(
                    "fichier incomplet : {received} octets reçus, {size} annoncés par le serveur"
                );
            }
            Ok(hex::encode(hasher.finalize()))
        })?;
        file.sync_all()?;
        if !digest.eq_ignore_ascii_case(sha256.trim()) {
            // Sans l'empreinte reçue : ce message arrive jusqu'à l'app, dans `status.json`.
            bail!("SHA-256 du fichier téléchargé différent de celui annoncé ({sha256})");
        }
        Ok(())
    }
}

/// `url` est-elle une adresse de `server` ?
fn same_origin(server: &ServerUrl, url: &reqwest::Url) -> bool {
    reqwest::Url::parse(server.as_str()).is_ok_and(|base| same_server(&base, url))
}

/// `to` est-elle une adresse du serveur de `from` ? Le même schéma, le même hôte et le même
/// port ; ou le même hôte en https sur le port 443, depuis http : une stack installée en http sur
/// un serveur qui sert en https. Un autre port du même hôte est un autre service.
fn same_server(from: &reqwest::Url, to: &reqwest::Url) -> bool {
    from.host_str() == to.host_str()
        && ((from.scheme() == to.scheme()
            && from.port_or_known_default() == to.port_or_known_default())
            || (from.scheme() == "http"
                && to.scheme() == "https"
                && to.port_or_known_default() == Some(443)))
}

/// Corps d'une réponse à une vérification, refusé au-delà de [`MAX_CHECK_BYTES`].
async fn limited_body(server: &ServerUrl, mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| transport(server, error))?
    {
        if body.len() + chunk.len() > MAX_CHECK_BYTES {
            bail!(
                "le serveur {server} a répondu plus de {} à la vérification",
                am_protocol::human_size(MAX_CHECK_BYTES as u64)
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn transport(server: &ServerUrl, error: reqwest::Error) -> anyhow::Error {
    let mut message = format!("serveur {server} injoignable");
    if error.is_timeout() {
        message.push_str(" (délai dépassé)");
    } else if error.is_connect() {
        message.push_str(" (connexion impossible)");
    }
    anyhow::Error::new(error).context(message)
}

fn api_error(server: &ServerUrl, status: StatusCode, body: &[u8]) -> anyhow::Error {
    match serde_json::from_slice::<ApiError>(body) {
        Ok(error) => anyhow!(
            "le serveur {server} refuse la vérification ({status}, {}) : {}",
            error.error,
            error.message
        ),
        Err(_) => anyhow!("le serveur {server} a répondu {status} à la vérification"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_stays_on_the_server_of_the_stack() {
        let server = ServerUrl::parse("https://updates.drakbase.com").unwrap();
        let same = |url: &str| same_origin(&server, &reqwest::Url::parse(url).unwrap());
        assert!(same(
            "https://updates.drakbase.com/dl/01ABC/demo.stack.tar.gz"
        ));
        assert!(same("https://updates.drakbase.com:443/dl/x"));
        for elsewhere in [
            "http://updates.drakbase.com/dl/x",
            "https://updates.drakbase.com:8443/dl/x",
            "https://updates.drakbase.com.exemple.fr/dl/x",
            "http://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1/dl/x",
            "file:///etc/shadow",
        ] {
            assert!(!same(elsewhere), "{elsewhere}");
        }
        // Installée en http sur un serveur qui sert en https : le même hôte, pas un autre.
        let plain = ServerUrl::parse("http://maj.exemple.fr").unwrap();
        let upgraded = reqwest::Url::parse("https://maj.exemple.fr/dl/x").unwrap();
        assert!(same_origin(&plain, &upgraded));
        for elsewhere in [
            "https://autre.exemple.fr/dl/x",
            // Un autre port du même hôte est un autre service.
            "https://maj.exemple.fr:8443/dl/x",
            "http://maj.exemple.fr:8080/dl/x",
        ] {
            let url = reqwest::Url::parse(elsewhere).unwrap();
            assert!(!same_origin(&plain, &url), "{elsewhere}");
        }
        let local = ServerUrl::parse("http://127.0.0.1:8790").unwrap();
        let url = reqwest::Url::parse("http://127.0.0.1:8790/dl/x").unwrap();
        assert!(same_origin(&local, &url));
    }
}
