//! La configuration d'une app Tauri, telle que la lit `tauri build` : `tauri.conf.json`, dans
//! lequel le fichier propre à une plateforme (`tauri.macos.conf.json`, `tauri.windows.conf.json`,
//! `tauri.linux.conf.json`) est fusionné pour les builds de cette plateforme. Un réglage n'a donc
//! pas forcément la même valeur d'un OS à l'autre.
//!
//! `publish` y lit la clé publique qu'embarque une version, `plugins.updater.pubkey`, qu'il
//! déclare dans `trusted_key_ids` (docs/API.md) : sans elle, une version publiée bloquerait ses
//! utilisateurs au premier changement de clé. `init` et `doctor` y lisent les réglages des mises
//! à jour, OS par OS.

use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::Os;
use am_protocol::signature::{KeyId, PublicKey};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// Configuration principale d'une app Tauri.
const CONFIG_FILE: &str = "tauri.conf.json";
/// Où la chercher dans le dossier du projet, dans l'ordre.
const PROJECT_CONFIGS: [&str; 2] = ["src-tauri/tauri.conf.json", CONFIG_FILE];
/// Les OS pour lesquels une app Tauri se construit, dans l'ordre des messages.
pub const OSES: [Os; 3] = [Os::Darwin, Os::Windows, Os::Linux];

/// La clé publique avec laquelle le plugin vérifie les mises à jour.
pub const PUBKEY: &str = "/plugins/updater/pubkey";
/// Les adresses de vérification.
pub const ENDPOINTS: &str = "/plugins/updater/endpoints";
/// Le certificat de signature de code des builds macOS.
pub const SIGNING_IDENTITY: &str = "/bundle/macOS/signingIdentity";

/// La configuration à lire : `explicit` (`--tauri-config`), le fichier ou le dossier qui le
/// contient, sinon celle du projet dans le dossier courant. `None` si le dossier courant n'en a
/// pas.
pub fn find_config(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    find_config_in(Path::new(""), explicit)
}

/// [`find_config`], avec le projet dans `dir` plutôt que dans le dossier courant.
pub fn find_config_in(dir: &Path, explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = explicit else {
        return Ok(in_project(dir));
    };
    if path.is_file() {
        return Ok(Some(path.to_owned()));
    }
    if !path.is_dir() {
        bail!("--tauri-config : {} introuvable", path.display());
    }
    in_project(path).map(Some).with_context(|| {
        format!(
            "--tauri-config : ni src-tauri/{CONFIG_FILE} ni {CONFIG_FILE} dans {}",
            path.display()
        )
    })
}

fn in_project(dir: &Path) -> Option<PathBuf> {
    PROJECT_CONFIGS
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// Fichier qui complète la configuration pour les builds d'un OS.
fn platform_config(os: Os) -> &'static str {
    match os {
        Os::Darwin => "tauri.macos.conf.json",
        Os::Windows => "tauri.windows.conf.json",
        Os::Linux => "tauri.linux.conf.json",
    }
}

/// Le nom d'un OS dans les messages.
pub fn os_name(os: Os) -> &'static str {
    match os {
        Os::Darwin => "macOS",
        Os::Windows => "Windows",
        Os::Linux => "Linux",
    }
}

/// La configuration d'une app Tauri pour chaque OS : `tauri.conf.json`, et les fichiers de
/// plateforme qui existent, fusionnés chacun dans la configuration comme le fait `tauri build`.
pub struct TauriConfig {
    /// `tauri.conf.json`.
    path: PathBuf,
    main: Value,
    platforms: Vec<Platform>,
}

/// Le fichier d'une plateforme.
struct Platform {
    os: Os,
    path: PathBuf,
    /// Ce que dit le fichier.
    patch: Value,
    /// La configuration une fois le fichier fusionné.
    merged: Value,
}

/// Un réglage, pour les builds d'un OS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setting<'a> {
    /// Sa valeur ; `None` s'il est absent, ou `null`.
    pub value: Option<&'a Value>,
    /// Le fichier qui le donne : celui de la plateforme s'il le change, sinon `tauri.conf.json`.
    pub source: &'a Path,
}

impl Setting<'_> {
    pub fn as_str(&self) -> Option<&str> {
        self.value.and_then(Value::as_str)
    }
}

impl TauriConfig {
    /// Lit `tauri.conf.json` et les fichiers de plateforme voisins.
    pub fn read(config: &Path) -> Result<Self> {
        Self::read_for(config, &OSES)
    }

    /// [`TauriConfig::read`], avec les fichiers de plateforme de ces OS seulement : `tauri build`
    /// ne lit pas ceux des autres.
    pub fn read_for(config: &Path, oses: &[Os]) -> Result<Self> {
        let main = read_json(config)?;
        let dir = config.parent().unwrap_or(Path::new(""));
        let mut platforms = Vec::new();
        for os in OSES {
            let path = dir.join(platform_config(os));
            if !oses.contains(&os) || !path.is_file() {
                continue;
            }
            let patch = read_json(&path)?;
            let mut merged = main.clone();
            merge(&mut merged, &patch);
            platforms.push(Platform {
                os,
                path,
                patch,
                merged,
            });
        }
        Ok(Self {
            path: config.to_owned(),
            main,
            platforms,
        })
    }

    /// `tauri.conf.json`, le seul fichier qu'`init` modifie.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Le réglage au bout de `pointer` (`/plugins/updater/pubkey`) pour les builds de cet OS.
    pub fn get(&self, os: Os, pointer: &str) -> Setting<'_> {
        let (config, source) = match self.platforms.iter().find(|platform| platform.os == os) {
            Some(platform) if changes(&platform.patch, pointer) => {
                (&platform.merged, platform.path.as_path())
            }
            _ => (&self.main, self.path.as_path()),
        };
        Setting {
            value: config.pointer(pointer).filter(|value| !value.is_null()),
            source,
        }
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("lecture de {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("{} n'est pas un JSON valide", path.display()))
}

/// Fusion JSON (RFC 7396), celle de `tauri build` : un objet complète l'objet du même nom, `null`
/// retire la clé, et toute autre valeur remplace celle d'avant, tableaux compris.
fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

/// Vrai si le fichier d'une plateforme change le réglage au bout de `pointer` : il le donne, le
/// retire, ou remplace d'un bloc l'objet qui le contient.
fn changes(patch: &Value, pointer: &str) -> bool {
    let mut current = patch;
    for key in pointer.split('/').skip(1) {
        match current {
            Value::Object(map) => match map.get(key) {
                Some(value) => current = value,
                None => return false,
            },
            _ => return true,
        }
    }
    true
}

/// La clé qu'embarquent les builds de ces OS, et le fichier qui la donne. Refuse une configuration
/// sans clé lisible, et des OS qui n'embarquent pas la même clé : une version ne déclare qu'une
/// liste de clés pour tous ses artefacts.
pub fn embedded_key(config: &Path, oses: &[Os]) -> Result<(KeyId, PathBuf)> {
    let config = TauriConfig::read_for(config, oses)?;
    let mut settings: Vec<Setting<'_>> = OSES
        .iter()
        .filter(|os| oses.contains(os))
        .map(|os| config.get(*os, PUBKEY))
        .collect();
    if settings.is_empty() {
        // Sans fichier de plateforme lu, c'est la clé de `tauri.conf.json`.
        settings.push(config.get(Os::Darwin, PUBKEY));
    }

    let mut found: Option<(KeyId, &Path)> = None;
    for setting in settings {
        let source = setting.source;
        let text = match setting.value {
            None => bail!(
                "{} n'a pas de plugins.updater.pubkey, la clé publique avec laquelle le plugin vérifie les mises à jour",
                source.display()
            ),
            Some(Value::String(text)) => text,
            Some(_) => bail!(
                "{} : plugins.updater.pubkey doit être une chaîne",
                source.display()
            ),
        };
        let key = PublicKey::from_tauri(text.trim())
            .with_context(|| {
                format!(
                    "{} : plugins.updater.pubkey n'est pas une clé publique au format Tauri",
                    source.display()
                )
            })?
            .key_id();
        match &found {
            Some((first, origin)) if *first != key => bail!(
                "les builds de cette version n'embarquent pas la même clé : {} donne {first}, {} donne {key}. Une version déclare la même clé embarquée pour tous ses artefacts : publie-les dans des versions séparées, ou garde la même clé partout.",
                origin.display(),
                source.display()
            ),
            Some(_) => {}
            None => found = Some((key, source)),
        }
    }
    found
        .map(|(key, source)| (key, source.to_owned()))
        .context("aucune configuration à lire")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_key(name: &str) -> String {
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/keys/{name}.pub")),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    fn key_id(name: &str) -> KeyId {
        PublicKey::from_tauri(&fixture_key(name)).unwrap().key_id()
    }

    fn write_config(path: &Path, config: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, config.to_string()).unwrap();
    }

    fn with_key(key: &str) -> Value {
        serde_json::json!({ "identifier": "fr.example.demo", "plugins": { "updater": { "pubkey": key } } })
    }

    #[test]
    fn the_configuration_is_found_from_a_file_or_a_folder() {
        let project = tempfile::tempdir().unwrap();
        let config = project.path().join("src-tauri/tauri.conf.json");
        write_config(&config, with_key(&fixture_key("test-app")));

        for given in [
            project.path().to_owned(),
            project.path().join("src-tauri"),
            config.clone(),
        ] {
            assert_eq!(find_config(Some(&given)).unwrap(), Some(config.clone()));
        }
        let empty = tempfile::tempdir().unwrap();
        let error = find_config(Some(empty.path())).unwrap_err().to_string();
        assert!(error.contains("ni src-tauri/tauri.conf.json"), "{error}");
        let missing = find_config(Some(&empty.path().join("absent.json")))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("introuvable"), "{missing}");
    }

    #[test]
    fn a_platform_file_replaces_the_key_for_its_builds() {
        let project = tempfile::tempdir().unwrap();
        let dir = project.path().join("src-tauri");
        let config = dir.join("tauri.conf.json");
        write_config(&config, with_key(&fixture_key("test-app")));
        let windows = dir.join("tauri.windows.conf.json");
        write_config(&windows, with_key(&fixture_key("other-app")));
        // Un fichier de plateforme qui ne touche pas à la clé laisse celle de la configuration.
        write_config(
            &dir.join("tauri.linux.conf.json"),
            serde_json::json!({ "bundle": { "targets": ["appimage"] } }),
        );

        assert_eq!(
            embedded_key(&config, &[Os::Darwin, Os::Linux]).unwrap(),
            (key_id("test-app"), config.clone())
        );
        assert_eq!(
            embedded_key(&config, &[Os::Windows]).unwrap(),
            (key_id("other-app"), windows)
        );
        assert_eq!(
            embedded_key(&config, &[]).unwrap(),
            (key_id("test-app"), config.clone())
        );
        let mixed = embedded_key(&config, &[Os::Darwin, Os::Windows])
            .unwrap_err()
            .to_string();
        assert!(mixed.contains("pas la même clé"), "{mixed}");
    }

    #[test]
    fn configurations_without_a_readable_key_are_refused() {
        let project = tempfile::tempdir().unwrap();
        let config = project.path().join("tauri.conf.json");
        for (contents, expected) in [
            (
                serde_json::json!({ "plugins": { "updater": { "endpoints": [] } } }).to_string(),
                "n'a pas de plugins.updater.pubkey",
            ),
            (
                serde_json::json!({ "plugins": { "updater": { "pubkey": 3 } } }).to_string(),
                "doit être une chaîne",
            ),
            (
                with_key("RWQ pas une clé").to_string(),
                "pas une clé publique au format Tauri",
            ),
            ("{ \"plugins\": ".to_owned(), "pas un JSON valide"),
        ] {
            fs::write(&config, contents).unwrap();
            let error = format!("{:#}", embedded_key(&config, &[Os::Darwin]).unwrap_err());
            assert!(error.contains(expected), "{expected}\n{error}");
        }
    }

    #[test]
    fn platform_files_are_merged_like_tauri_build() {
        let project = tempfile::tempdir().unwrap();
        let dir = project.path().join("src-tauri");
        let config = dir.join("tauri.conf.json");
        write_config(
            &config,
            serde_json::json!({
                "identifier": "fr.example.demo",
                "bundle": { "targets": ["app", "dmg"], "createUpdaterArtifacts": true },
                "plugins": { "updater": { "pubkey": "principale", "endpoints": ["a"] } },
            }),
        );
        let macos = dir.join("tauri.macos.conf.json");
        write_config(
            &macos,
            serde_json::json!({
                "bundle": { "macOS": { "signingIdentity": "Mon certificat" }, "targets": ["app"] },
                "plugins": { "updater": { "endpoints": null } },
            }),
        );
        let windows = dir.join("tauri.windows.conf.json");
        write_config(&windows, serde_json::json!({ "plugins": null }));

        let merged = TauriConfig::read(&config).unwrap();
        // Le fichier de la plateforme donne le réglage : c'est lui la source.
        let identity = merged.get(Os::Darwin, SIGNING_IDENTITY);
        assert_eq!(identity.as_str(), Some("Mon certificat"));
        assert_eq!(identity.source, macos);
        // Les autres OS ne le lisent pas.
        for os in [Os::Windows, Os::Linux] {
            let other = merged.get(os, SIGNING_IDENTITY);
            assert_eq!((other.value, other.source), (None, config.as_path()));
        }
        // Un objet complète l'objet du même nom : ce qu'il ne dit pas reste celui de
        // tauri.conf.json.
        let pubkey = merged.get(Os::Darwin, PUBKEY);
        assert_eq!(
            (pubkey.as_str(), pubkey.source),
            (Some("principale"), config.as_path())
        );
        let artifacts = merged.get(Os::Darwin, "/bundle/createUpdaterArtifacts");
        assert_eq!(artifacts.value, Some(&Value::Bool(true)));
        assert_eq!(artifacts.source, config);
        // Un tableau remplace l'autre, `null` retire la clé.
        assert_eq!(
            merged.get(Os::Darwin, "/bundle/targets").value,
            Some(&serde_json::json!(["app"]))
        );
        let endpoints = merged.get(Os::Darwin, ENDPOINTS);
        assert_eq!((endpoints.value, endpoints.source), (None, macos.as_path()));
        assert_eq!(
            merged.get(Os::Linux, ENDPOINTS).value,
            Some(&serde_json::json!(["a"]))
        );
        // Un objet retiré d'un bloc emporte ce qu'il contenait.
        let removed = merged.get(Os::Windows, PUBKEY);
        assert_eq!((removed.value, removed.source), (None, windows.as_path()));
        assert_eq!(
            merged.get(Os::Windows, "/identifier").as_str(),
            Some("fr.example.demo")
        );

        // `publish` ne lit que les fichiers des OS de ses artefacts : celui d'un autre OS, même
        // illisible, ne le gêne pas.
        fs::write(&windows, "{ pas du JSON").unwrap();
        assert!(TauriConfig::read(&config).is_err());
        let linux_only = TauriConfig::read_for(&config, &[Os::Linux]).unwrap();
        assert_eq!(
            linux_only.get(Os::Linux, PUBKEY).as_str(),
            Some("principale")
        );
    }
}
