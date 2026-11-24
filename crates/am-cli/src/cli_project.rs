//! Une app `cli` (docs/PROTOCOLE.md, « Exécutable `bin` ») : un exécutable Linux, comme l'agent
//! `drakbase-stack`. Elle embarque ses clés publiques, lues à la compilation dans `drakbase.json`,
//! à la racine du projet :
//!
//! ```json
//! {
//!   "app": "drakbase-stack",
//!   "public_keys": ["<clé principale au format Tauri>", "<clé de secours>"]
//! }
//! ```
//!
//! `drakbase init <app> --cli` l'écrit ; `drakbase publish` y lit les clés qu'une version
//! embarque (`trusted_key_ids`), et `doctor` le contrôle.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::signature::{KeyId, PublicKey};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use crate::project;

/// Le nom du fichier, à la racine du projet.
pub const FILE: &str = "drakbase.json";
/// Comme pour `stack.json` : de 1 à 4 clés.
const MAX_KEYS: usize = 4;

/// Le projet d'une app `cli` : son `drakbase.json`.
#[derive(Clone, Debug)]
pub struct CliProject {
    pub root: PathBuf,
    pub file: PathBuf,
}

impl CliProject {
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            file: root.join(FILE),
        }
    }

    /// Le projet de `root`, s'il a un `drakbase.json`.
    pub fn find(root: &Path) -> Option<Self> {
        let project = Self::at(root);
        project.file.is_file().then_some(project)
    }
}

/// Ce que dit `drakbase.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliManifest {
    pub app: Option<String>,
    /// Au format Tauri, telles qu'écrites, la principale d'abord.
    pub public_keys: Vec<String>,
}

impl CliManifest {
    /// Lit et contrôle `drakbase.json` : un objet, `app` en texte, `public_keys` de 1 à 4 clés
    /// lisibles, d'identifiants distincts.
    pub fn read(path: &Path) -> Result<Self> {
        let name = project::display(path);
        let text = fs::read_to_string(path).with_context(|| format!("lecture de {name}"))?;
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("{name} n'est pas un JSON valide"))?;
        let object = value
            .as_object()
            .with_context(|| format!("{name} : un objet JSON est attendu"))?;
        let app = match object.get("app") {
            None | Some(Value::Null) => None,
            Some(Value::String(app)) => Some(app.clone()),
            Some(_) => bail!("{name} : app doit être un texte"),
        };
        let public_keys: Vec<String> = match object.get("public_keys") {
            None => bail!("{name} : public_keys manque (drakbase init <app> --cli l'écrit)"),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<_>>()
                .with_context(|| {
                    format!("{name} : public_keys contient autre chose que des textes")
                })?,
            Some(_) => bail!("{name} : public_keys doit être une liste"),
        };
        ensure!(
            (1..=MAX_KEYS).contains(&public_keys.len()),
            "{name} : de 1 à {MAX_KEYS} clés publiques dans public_keys"
        );
        let mut seen = BTreeSet::new();
        for (index, key) in public_keys.iter().enumerate() {
            let id = PublicKey::from_tauri(key)
                .with_context(|| format!("{name} : clé publique n° {} illisible", index + 1))?
                .key_id();
            ensure!(seen.insert(id.to_hex()), "{name} : clé {id} en double");
        }
        Ok(Self { app, public_keys })
    }

    /// Les identifiants des clés : les `trusted_key_ids` d'une version.
    pub fn key_ids(&self) -> Vec<KeyId> {
        self.public_keys
            .iter()
            .filter_map(|key| PublicKey::from_tauri(key).ok())
            .map(|key| key.key_id())
            .collect()
    }
}

/// `drakbase.json` à lire : `given` (le fichier, ou son dossier), sinon celui du dossier courant.
/// `None` s'il n'existe pas.
pub fn locate(given: Option<&Path>) -> Result<Option<PathBuf>> {
    match given {
        Some(path) if path.is_dir() => {
            let file = path.join(FILE);
            ensure!(file.is_file(), "{} introuvable", file.display());
            Ok(Some(file))
        }
        Some(path) => {
            ensure!(path.is_file(), "{} introuvable", path.display());
            Ok(Some(path.to_owned()))
        }
        None => Ok(Some(PathBuf::from(FILE)).filter(|path| path.is_file())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> String {
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/keys/{name}.pub")),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    #[test]
    fn the_manifest_gives_the_trusted_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let (main, backup) = (key("test-app"), key("other-app"));
        fs::write(
            &path,
            serde_json::to_string(
                &serde_json::json!({"app": "drakbase-stack", "public_keys": [main, backup]}),
            )
            .unwrap(),
        )
        .unwrap();
        let manifest = CliManifest::read(&path).unwrap();
        assert_eq!(manifest.app.as_deref(), Some("drakbase-stack"));
        let ids: Vec<KeyId> = [&main, &backup]
            .iter()
            .map(|key| PublicKey::from_tauri(key).unwrap().key_id())
            .collect();
        assert_eq!(manifest.key_ids(), ids);
        assert_eq!(locate(Some(dir.path())).unwrap(), Some(path.clone()));

        for (contents, message) in [
            ("[]", "un objet JSON est attendu"),
            ("{\"app\": \"x\"}", "public_keys manque"),
            ("{\"public_keys\": []}", "de 1 à 4 clés"),
            (
                "{\"public_keys\": [\"abc\"]}",
                "clé publique n° 1 illisible",
            ),
            (
                &format!("{{\"public_keys\": [\"{main}\", \"{main}\"]}}"),
                "en double",
            ),
            (
                "{\"app\": 3, \"public_keys\": []}",
                "app doit être un texte",
            ),
        ] {
            fs::write(&path, contents).unwrap();
            let error = format!("{:#}", CliManifest::read(&path).unwrap_err());
            assert!(error.contains(message), "{contents} : {error}");
        }
        let missing = dir.path().join("rien");
        assert!(locate(Some(&missing)).is_err());
    }
}
