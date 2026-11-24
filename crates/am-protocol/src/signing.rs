//! Génération de clés et signature au format Tauri, pour l'outil `drakbase` et les jeux d'essai.
//! Le serveur ne signe jamais rien.

use std::io::Read;

use crate::Version;
use crate::signature::{KeyId, PublicKey, SignatureError, decode_tauri, encode_tauri};

/// Commentaire non signé qu'écrit `tauri signer`.
const UNTRUSTED_COMMENT: &str = "signature from tauri secret key";

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("clé secrète illisible, ou mot de passe faux")]
    SecretKey,
    #[error("génération de la paire de clés impossible : {0}")]
    Generate(String),
    #[error("nom de fichier « {0} » : tabulation ou retour à la ligne interdits")]
    FileName(String),
    #[error("signature impossible : {0}")]
    Sign(String),
    #[error(transparent)]
    Signature(#[from] SignatureError),
}

/// Paire de clés au format Tauri. `public` va dans l'app et dans l'administration ; `secret`,
/// chiffré par mot de passe, reste sur le poste du développeur.
#[derive(Debug)]
pub struct TauriKeyPair {
    pub public: String,
    pub secret: String,
    pub key_id: KeyId,
}

/// Crée une paire de clés dont la clé secrète est chiffrée par `password`, comme `tauri signer generate`.
pub fn generate_keypair(password: &str) -> Result<TauriKeyPair, SigningError> {
    let minisign::KeyPair { pk, sk } =
        minisign::KeyPair::generate_encrypted_keypair(Some(password.to_owned()))
            .map_err(|error| SigningError::Generate(error.to_string()))?;
    let public_box = pk
        .to_box()
        .map_err(|error| SigningError::Generate(error.to_string()))?;
    let secret_box = sk
        .to_box(None)
        .map_err(|error| SigningError::Generate(error.to_string()))?;
    let public = encode_tauri(&public_box.to_string());
    let secret = encode_tauri(&secret_box.to_string());
    let key_id = PublicKey::from_tauri(&public)?.key_id();
    Ok(TauriKeyPair {
        public,
        secret,
        key_id,
    })
}

/// Ouvre une clé secrète au format Tauri avec son mot de passe, et rend son identifiant : de quoi
/// vérifier qu'une sauvegarde de la clé et de son mot de passe sert encore, sans rien signer.
pub fn secret_key_id(secret: &str, password: &str) -> Result<KeyId, SigningError> {
    let secret_key = minisign::SecretKeyBox::from_string(&decode_tauri(secret)?)
        .and_then(|secret_box| secret_box.into_secret_key(Some(password.to_owned())))
        .map_err(|_| SigningError::SecretKey)?;
    let bytes: [u8; 8] = secret_key
        .keynum()
        .try_into()
        .map_err(|_| SigningError::SecretKey)?;
    Ok(KeyId::from_bytes(bytes))
}

/// Signe `data` comme le fait Tauri, avec la version canonique dans le commentaire de confiance.
/// Rend le contenu du `.sig`.
pub fn sign(
    secret: &str,
    password: &str,
    data: impl Read,
    file_name: &str,
    version: &Version,
    timestamp: u64,
) -> Result<String, SigningError> {
    if file_name.contains(['\t', '\n', '\r']) {
        return Err(SigningError::FileName(file_name.to_owned()));
    }
    let secret_key = minisign::SecretKeyBox::from_string(&decode_tauri(secret)?)
        .and_then(|secret_box| secret_box.into_secret_key(Some(password.to_owned())))
        .map_err(|_| SigningError::SecretKey)?;
    let trusted_comment = format!("timestamp:{timestamp}\tfile:{file_name}\tversion:{version}");
    let signature = minisign::sign(
        None,
        &secret_key,
        data,
        Some(&trusted_comment),
        Some(UNTRUSTED_COMMENT),
    )
    .map_err(|error| SigningError::Sign(error.to_string()))?;
    Ok(encode_tauri(&signature.into_string()))
}
