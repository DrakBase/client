//! Contrôle de la signature d'un artefact avec les clés de confiance, dans l'ordre de
//! docs/PROTOCOLE.md, « Signatures » : algorithme, identifiant de clé, signature du fichier,
//! signature globale, puis version signée, égale à celle annoncée.

use am_protocol::Version;
use am_protocol::signature::{KeyId, PublicKey, Signature};
use anyhow::{Context, Result, anyhow};

/// Vérifie `data` avec celle des clés `trusted` (au format Tauri) dont l'identifiant est celui de
/// la signature, et rend cet identifiant.
pub fn verify(
    trusted: &[String],
    data: &[u8],
    signature: &str,
    version: &Version,
) -> Result<KeyId> {
    let signature = Signature::from_tauri(signature).context("signature du serveur illisible")?;
    let keys: Vec<PublicKey> = trusted
        .iter()
        .filter_map(|key| PublicKey::from_tauri(key).ok())
        .collect();
    let key = keys
        .iter()
        .find(|key| key.key_id() == signature.key_id())
        .ok_or_else(|| {
            let known: Vec<String> = keys.iter().map(|key| key.key_id().to_hex()).collect();
            anyhow!(
                "signée avec la clé {}, qui n'est pas une clé de confiance de cette installation ({})",
                signature.key_id(),
                if known.is_empty() {
                    "aucune clé".to_owned()
                } else {
                    known.join(", ")
                }
            )
        })?;
    let verified = key.verify(data, &signature).context("signature refusée")?;
    verified
        .require_version(version)
        .context("signature refusée")?;
    Ok(verified.key_id)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use am_protocol::signing;

    use super::*;

    fn fixture(name: &str) -> String {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/keys")
            .join(name);
        fs::read_to_string(path).unwrap().trim().to_owned()
    }

    fn sign(secret: &str, data: &[u8], version: &str) -> String {
        signing::sign(
            &fixture(secret),
            "appsmarket-fixtures",
            data,
            "demo.stack.tar.gz",
            &Version::parse_release(version).unwrap(),
            1_767_225_600,
        )
        .unwrap()
    }

    #[test]
    fn checks_key_signature_and_version() {
        let trusted = vec![fixture("other-app.pub"), fixture("test-app.pub")];
        let data = b"stack";
        let version = Version::parse_release("1.1.0").unwrap();
        let signature = sign("test-app.secret", data, "1.1.0");
        let key_id = verify(&trusted, data, &signature, &version).unwrap();
        assert_eq!(
            key_id,
            PublicKey::from_tauri(&fixture("test-app.pub"))
                .unwrap()
                .key_id()
        );

        // Fichier modifié.
        let error = verify(&trusted, b"stacK", &signature, &version).unwrap_err();
        assert!(
            format!("{error:#}").contains("signature invalide"),
            "{error:#}"
        );
        // Version signée différente de celle annoncée.
        let error = verify(
            &trusted,
            data,
            &signature,
            &Version::parse_release("1.2.0").unwrap(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("version signée"), "{error:#}");
        // Clé inconnue de l'installation.
        let error = verify(&trusted[..1], data, &signature, &version).unwrap_err();
        assert!(
            error.to_string().contains("pas une clé de confiance"),
            "{error}"
        );
    }
}
