//! Vérification des signatures minisign au format Tauri : textes minisign encodés en base64 standard.
//!
//! L'ordre des contrôles suit `docs/PROTOCOLE.md` : algorithme préhaché, identifiant de clé,
//! signature du fichier, signature globale, et seulement ensuite la version du commentaire de
//! confiance, qui n'est authentifié que par la signature globale.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::Version;

const ALG_PREHASHED: [u8; 2] = *b"ED";
const ALG_LEGACY: [u8; 2] = *b"Ed";
const ALG_PUBLIC_KEY: [u8; 2] = *b"Ed";
const PUBLIC_KEY_LINE_LEN: usize = 2 + 8 + 32;
const SIGNATURE_LINE_LEN: usize = 2 + 8 + 64;
const UNTRUSTED_COMMENT_PREFIX: &str = "untrusted comment:";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("encodage base64 invalide")]
    Base64,
    #[error("texte minisign non UTF-8")]
    Utf8,
    #[error("clé publique minisign illisible")]
    MalformedPublicKey,
    #[error("signature minisign illisible")]
    MalformedSignature,
    #[error("signature au format historique non préhaché : refusée")]
    Legacy,
    #[error("signature faite avec la clé {found}, attendue {expected}")]
    KeyIdMismatch { expected: KeyId, found: KeyId },
    #[error("signature invalide")]
    Invalid,
    #[error("le commentaire de confiance ne contient pas de version")]
    MissingVersion,
    #[error("version signée « {signed} », attendue « {expected} »")]
    VersionMismatch { signed: String, expected: String },
}

/// Identifiant de clé minisign (8 octets).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyId([u8; 8]);

impl KeyId {
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    /// Forme affichée par minisign : entier 64 bits petit-boutiste, en hexadécimal majuscule.
    pub fn to_hex(self) -> String {
        format!("{:016X}", u64::from_le_bytes(self.0))
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({})", self.to_hex())
    }
}

/// Décode l'enveloppe Tauri (base64 standard) en texte minisign.
pub fn decode_tauri(encoded: &str) -> Result<String, SignatureError> {
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| SignatureError::Base64)?;
    String::from_utf8(bytes).map_err(|_| SignatureError::Utf8)
}

/// Met un texte minisign dans l'enveloppe Tauri.
pub fn encode_tauri(text: &str) -> String {
    STANDARD.encode(text)
}

/// Ligne de données d'un texte minisign, décodée.
///
/// Le texte est découpé exactement comme le fait `minisign_verify` (`str::lines`, deuxième ligne
/// telle quelle, sans rogner ni sauter de ligne) : l'identifiant de clé et l'algorithme lus ici
/// viennent donc de la ligne que la crate vérifie. La première ligne doit être le commentaire non
/// signé, ce qui écarte un texte décalé par une ligne vide.
fn data_line(text: &str) -> Option<Vec<u8>> {
    let mut lines = text.lines();
    if !lines.next()?.starts_with(UNTRUSTED_COMMENT_PREFIX) {
        return None;
    }
    STANDARD.decode(lines.next()?).ok()
}

fn key_id_at(bytes: &[u8]) -> KeyId {
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[2..10]);
    KeyId(id)
}

/// Clé publique d'une app.
pub struct PublicKey {
    inner: minisign_verify::PublicKey,
    key_id: KeyId,
}

impl PublicKey {
    /// Lit une clé au format Tauri, comme la valeur de `plugins.updater.pubkey`.
    pub fn from_tauri(encoded: &str) -> Result<Self, SignatureError> {
        Self::from_minisign(&decode_tauri(encoded)?)
    }

    /// Lit le texte minisign d'une clé publique : commentaire non signé, puis ligne de données.
    pub fn from_minisign(text: &str) -> Result<Self, SignatureError> {
        let bytes = data_line(text).ok_or(SignatureError::MalformedPublicKey)?;
        if bytes.len() != PUBLIC_KEY_LINE_LEN || bytes[..2] != ALG_PUBLIC_KEY {
            return Err(SignatureError::MalformedPublicKey);
        }
        let inner = minisign_verify::PublicKey::decode(text)
            .map_err(|_| SignatureError::MalformedPublicKey)?;
        Ok(Self {
            inner,
            key_id: key_id_at(&bytes),
        })
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// Vérifie une signature sur des données en mémoire.
    pub fn verify(&self, data: &[u8], signature: &Signature) -> Result<Verified, SignatureError> {
        let mut verifier = self.verify_stream(signature)?;
        verifier.update(data);
        verifier.finalize()
    }

    /// Commence une vérification en flux, pour les gros fichiers. Le format historique et une
    /// autre clé sont refusés avant de lire la moindre donnée.
    pub fn verify_stream<'a>(
        &'a self,
        signature: &'a Signature,
    ) -> Result<StreamVerifier<'a>, SignatureError> {
        if !signature.prehashed {
            return Err(SignatureError::Legacy);
        }
        if signature.key_id != self.key_id {
            return Err(SignatureError::KeyIdMismatch {
                expected: self.key_id,
                found: signature.key_id,
            });
        }
        let inner = self
            .inner
            .verify_stream(&signature.inner)
            .map_err(|_| SignatureError::Invalid)?;
        Ok(StreamVerifier { inner, signature })
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.key_id)
    }
}

/// Signature d'un fichier, lue depuis un `.sig` au format Tauri.
pub struct Signature {
    inner: minisign_verify::Signature,
    key_id: KeyId,
    prehashed: bool,
}

impl Signature {
    pub fn from_tauri(encoded: &str) -> Result<Self, SignatureError> {
        Self::from_minisign(&decode_tauri(encoded)?)
    }

    /// Lit le texte minisign d'une signature : commentaire non signé, ligne de données,
    /// commentaire de confiance, signature globale.
    pub fn from_minisign(text: &str) -> Result<Self, SignatureError> {
        let bytes = data_line(text).ok_or(SignatureError::MalformedSignature)?;
        if bytes.len() != SIGNATURE_LINE_LEN {
            return Err(SignatureError::MalformedSignature);
        }
        let prehashed = match [bytes[0], bytes[1]] {
            ALG_PREHASHED => true,
            ALG_LEGACY => false,
            _ => return Err(SignatureError::MalformedSignature),
        };
        let inner = minisign_verify::Signature::decode(text)
            .map_err(|_| SignatureError::MalformedSignature)?;
        Ok(Self {
            inner,
            key_id: key_id_at(&bytes),
            prehashed,
        })
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    pub fn is_prehashed(&self) -> bool {
        self.prehashed
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Signature({}, préhachée : {})",
            self.key_id, self.prehashed
        )
    }
}

/// Vérification en cours, alimentée morceau par morceau.
pub struct StreamVerifier<'a> {
    inner: minisign_verify::StreamVerifier<'a>,
    signature: &'a Signature,
}

impl StreamVerifier<'_> {
    pub fn update(&mut self, chunk: &[u8]) {
        self.inner.update(chunk);
    }

    /// Contrôle la signature du fichier puis la signature globale. Le commentaire de confiance
    /// n'est rendu qu'une fois authentifié.
    pub fn finalize(mut self) -> Result<Verified, SignatureError> {
        self.inner.finalize().map_err(|_| SignatureError::Invalid)?;
        Ok(Verified {
            key_id: self.signature.key_id,
            trusted_comment: self.signature.inner.trusted_comment().to_owned(),
        })
    }
}

/// Résultat d'une vérification réussie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub key_id: KeyId,
    pub trusted_comment: String,
}

impl Verified {
    /// Valeur d'un champ `clé:valeur` du commentaire de confiance (champs séparés par des tabulations).
    pub fn field(&self, name: &str) -> Option<&str> {
        self.trusted_comment
            .split('\t')
            .filter_map(|field| field.split_once(':'))
            .find_map(|(key, value)| (key == name).then_some(value))
    }

    /// Exige que la version signée soit exactement la version canonique attendue.
    pub fn require_version(&self, expected: &Version) -> Result<(), SignatureError> {
        let signed = self
            .field("version")
            .ok_or(SignatureError::MissingVersion)?;
        let expected = expected.to_string();
        if signed == expected {
            Ok(())
        } else {
            Err(SignatureError::VersionMismatch {
                signed: signed.to_owned(),
                expected,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_id_is_shown_as_minisign_shows_it() {
        let id = KeyId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(id.to_hex(), "0807060504030201");
        assert_eq!(id.to_string(), "0807060504030201");
    }

    /// Ligne de données synthétique d'une clé publique dont l'identifiant ne contient que l'octet `id`.
    fn key_line(id: u8) -> String {
        let mut data = ALG_PUBLIC_KEY.to_vec();
        data.extend([id; 8]);
        data.extend([9u8; 32]);
        STANDARD.encode(data)
    }

    #[test]
    fn reads_exactly_the_line_minisign_verify_reads() {
        let text = format!("untrusted comment: k\n{}\n", key_line(1));
        assert_eq!(
            PublicKey::from_minisign(&text).unwrap().key_id(),
            KeyId::from_bytes([1; 8])
        );
        // Fins de ligne Windows : `str::lines` retire le `\r`, comme minisign_verify.
        assert_eq!(
            PublicKey::from_minisign(&text.replace('\n', "\r\n"))
                .unwrap()
                .key_id(),
            KeyId::from_bytes([1; 8])
        );
        // Une ligne vide en tête décalait la lecture : l'identifiant venait d'une ligne, la clé de l'autre.
        for refused in [
            format!("\n{}\n{}\n", key_line(1), key_line(2)),
            format!("{}\n{}\n", key_line(1), key_line(1)),
            format!("   \n{}\n", key_line(1)),
        ] {
            assert_eq!(
                PublicKey::from_minisign(&refused).unwrap_err(),
                SignatureError::MalformedPublicKey,
                "{refused:?}"
            );
        }
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert_eq!(
            PublicKey::from_tauri("pas du base64 !").unwrap_err(),
            SignatureError::Base64
        );
        assert_eq!(
            Signature::from_minisign("untrusted comment: x\nQUJD\n").unwrap_err(),
            SignatureError::MalformedSignature
        );
    }

    #[test]
    fn trusted_comment_fields() {
        let verified = Verified {
            key_id: KeyId::from_bytes([0; 8]),
            trusted_comment: "timestamp:1767225600\tfile:a.app.tar.gz\tversion:1.2.0".to_owned(),
        };
        assert_eq!(verified.field("file"), Some("a.app.tar.gz"));
        assert!(
            verified
                .require_version(&Version::parse_release("1.2").unwrap())
                .is_ok()
        );
        assert_eq!(
            verified
                .require_version(&Version::parse_release("1.3.0").unwrap())
                .unwrap_err(),
            SignatureError::VersionMismatch {
                signed: "1.2.0".to_owned(),
                expected: "1.3.0".to_owned()
            }
        );
    }
}
