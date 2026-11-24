//! `/var/lib/drakbase-stack/<app>/state.json` : version installée, précédente, en échec et
//! écartées, canal, clés de confiance et permissions acceptées.

use std::collections::BTreeSet;
use std::fs;
use std::io;

use am_protocol::stack::compose::{ComposeReport, Permission};
use am_protocol::stack::control::{self as protocol_control, Available, Operation, Status};
use am_protocol::stack::is_app_slug;
use am_protocol::{Channel, ServerUrl, Version};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::control;
use crate::files;
use crate::paths::Paths;

pub const FORMAT: u32 = 1;

/// Une version gardée sur la machine, avec les images de son compose résolu : de quoi les
/// supprimer quand elle n'est plus gardée.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub version: Version,
    #[serde(default)]
    pub images: Vec<String>,
    /// Démarrage de cette version, en RFC 3339 UTC.
    pub since: String,
}

impl ReleaseRecord {
    pub fn new(version: Version, images: Vec<String>) -> Self {
        Self {
            version,
            images,
            since: now(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackState {
    pub format: u32,
    pub app: String,
    pub server: ServerUrl,
    pub channel: Channel,
    /// Version en place, la dernière devenue saine. Absente tant qu'une première installation
    /// n'a pas abouti.
    pub installed: Option<ReleaseRecord>,
    /// Version d'avant, gardée pour `rollback`.
    pub previous: Option<ReleaseRecord>,
    /// Version qui n'est pas devenue saine avec `rollback: manual` : ses conteneurs sont restés
    /// en l'état, jusqu'à `rollback` ou une mise à jour qui aboutit.
    pub failed: Option<ReleaseRecord>,
    /// Versions écartées après un échec : seul `update --retry` les reprend.
    pub rejected: Vec<Version>,
    /// Clés publiques au format Tauri qui vérifient la version suivante : la clé d'`install`,
    /// puis les `public_keys` de la dernière version installée.
    pub trusted_keys: Vec<String>,
    /// Clés des permissions acceptées (`<service>|<type>|<valeur>`), triées.
    pub accepted_permissions: Vec<String>,
    /// Dernière vérification réussie, en RFC 3339 UTC.
    #[serde(default)]
    pub checked_at: Option<String>,
    /// Version plus récente, examinée par `check` : celle que l'app propose.
    #[serde(default)]
    pub available: Option<Available>,
    /// Dernière demande de l'app, ou dernière vérification.
    #[serde(default)]
    pub operation: Option<Operation>,
}

impl StackState {
    pub fn new(app: &str, server: ServerUrl, channel: Channel, key: String) -> Self {
        Self {
            format: FORMAT,
            app: app.to_owned(),
            server,
            channel,
            installed: None,
            previous: None,
            failed: None,
            rejected: Vec::new(),
            trusted_keys: vec![key],
            accepted_permissions: Vec::new(),
            checked_at: None,
            available: None,
            operation: None,
        }
    }

    pub fn load(paths: &Paths, app: &str) -> Result<Option<Self>> {
        let path = paths.stack(app).state_file();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("lecture de {}", path.display()));
            }
        };
        let state: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} illisible", path.display()))?;
        if state.format != FORMAT {
            anyhow::bail!(
                "{} : format {} inconnu de cette version de drakbase-stack (self-update ?)",
                path.display(),
                state.format
            );
        }
        if state.app != app {
            anyhow::bail!("{} décrit « {} », pas « {app} »", path.display(), state.app);
        }
        Ok(Some(state))
    }

    /// Lit l'état d'une stack qui doit être installée.
    pub fn require(paths: &Paths, app: &str) -> Result<Self> {
        Self::load(paths, app)?.ok_or_else(|| {
            anyhow::anyhow!(
                "{app} n'est pas installée sur cette machine (drakbase-stack install {app} --key <clé>)"
            )
        })
    }

    /// Écrit `state.json`, dans le dossier de la stack, créé ou remis en 0700. Une stack qui a
    /// un dossier de contrôle y reçoit aussi `status.json`.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let stack = paths.stack(&self.app);
        control::close_stack_dir(&stack.dir)?;
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        files::replace(&stack.state_file(), text.as_bytes(), 0o644)?;
        // L'app lit son état dans le dossier de contrôle : il suit chaque changement.
        control::write_status(&stack, self)
    }

    /// Ce que l'app lit dans `status.json`.
    pub fn status(&self) -> Status {
        Status {
            format: protocol_control::FORMAT,
            app: self.app.clone(),
            installed: self.installed_version().cloned(),
            channel: self.channel,
            checked_at: self.checked_at.clone(),
            available: self.available.clone(),
            failed: self.failed.as_ref().map(|record| record.version.clone()),
            operation: self.operation.clone(),
        }
    }

    /// Oublie la version proposée quand elle est installée, ou dépassée.
    pub fn forget_available_up_to(&mut self, version: &Version) {
        if self
            .available
            .as_ref()
            .is_some_and(|available| &available.version <= version)
        {
            self.available = None;
        }
    }

    pub fn installed_version(&self) -> Option<&Version> {
        self.installed.as_ref().map(|record| &record.version)
    }

    pub fn is_rejected(&self, version: &Version) -> bool {
        self.rejected.contains(version)
    }

    pub fn reject(&mut self, version: &Version) {
        if !self.is_rejected(version) {
            self.rejected.push(version.clone());
            self.rejected.sort();
        }
    }

    pub fn accept(&mut self, keys: impl IntoIterator<Item = String>) {
        let mut accepted: BTreeSet<String> = self.accepted_permissions.drain(..).collect();
        accepted.extend(keys);
        self.accepted_permissions = accepted.into_iter().collect();
    }

    /// Les permissions d'un compose qui ne sont pas encore acceptées, dans l'ordre du rapport.
    pub fn new_permissions<'r>(&self, report: &'r ComposeReport) -> Vec<&'r Permission> {
        report
            .permissions
            .iter()
            .filter(|permission| !self.is_accepted(&permission.key()))
            .collect()
    }

    pub fn is_accepted(&self, key: &str) -> bool {
        self.accepted_permissions
            .iter()
            .any(|accepted| accepted == key)
    }

    /// Versions gardées sur la machine : installée, précédente et en échec.
    pub fn kept(&self) -> impl Iterator<Item = &ReleaseRecord> {
        [&self.installed, &self.previous, &self.failed]
            .into_iter()
            .flatten()
    }

    /// Images encore utilisées par une version gardée.
    pub fn kept_images(&self) -> BTreeSet<&str> {
        self.kept()
            .flat_map(|record| record.images.iter().map(String::as_str))
            .collect()
    }
}

/// Stacks installées, dans l'ordre de leur nom : les dossiers qui ont un `state.json`. Un dossier
/// de stack où l'on n'entre pas (il est en 0700, à root) est une erreur, pas une stack absente.
pub fn list(paths: &Paths) -> Result<Vec<String>> {
    let entries = match fs::read_dir(&paths.state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("lecture de {}", paths.state_dir.display()));
        }
    };
    let mut apps = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_app_slug(&name) {
            continue;
        }
        let state_file = paths.stack(&name).state_file();
        match fs::metadata(&state_file) {
            Ok(metadata) if metadata.is_file() => apps.push(name),
            Ok(_) => {}
            // Rien à cet endroit, ou un fichier à la place du dossier : pas une stack.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("lecture de {}", state_file.display()));
            }
        }
    }
    apps.sort();
    Ok(apps)
}

pub fn now() -> String {
    OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .ok()
        .and_then(|now| now.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse_release(text).unwrap()
    }

    fn paths(root: &std::path::Path) -> Paths {
        Paths {
            config_dir: root.join("etc"),
            state_dir: root.join("lib"),
            systemd_dir: root.join("systemd"),
        }
    }

    fn demo() -> StackState {
        let server = ServerUrl::parse("https://maj.example.fr").unwrap();
        StackState::new("demo", server, Channel::Stable, "clé".into())
    }

    #[test]
    fn save_keeps_the_stack_folder_private_and_creates_no_control_folder() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let stack = paths.stack("demo");
        let mode =
            |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        let state = demo();

        // Sans dossier de contrôle : ni `control/`, ni `status.json`.
        state.save(&paths).unwrap();
        assert_eq!(mode(&stack.dir), 0o700);
        assert!(!stack.control_dir().exists());

        // Un dossier laissé ouvert par un agent plus ancien est refermé.
        fs::set_permissions(&stack.dir, fs::Permissions::from_mode(0o755)).unwrap();
        state.save(&paths).unwrap();
        assert_eq!(mode(&stack.dir), 0o700);

        // Avec un dossier de contrôle : `status.json` y est écrit, et `request/` n'est pas créé.
        fs::create_dir(stack.control_dir()).unwrap();
        state.save(&paths).unwrap();
        assert!(stack.status_file().is_file());
        assert!(!stack.request_dir().exists());
        assert!(!stack.status_temp().exists());
    }

    #[test]
    fn listing_reports_a_stack_folder_it_cannot_enter() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        demo().save(&paths).unwrap();
        // Un fichier au nom d'une app, à la place d'un dossier : pas une stack.
        fs::write(paths.state_dir.join("fichier"), "x").unwrap();
        assert_eq!(list(&paths).unwrap(), ["demo"]);

        // Un `state.json` qui ne se lit pas, pour root comme pour un autre compte : un lien qui
        // se désigne lui-même. C'est une erreur, pas une stack absente.
        let looping = paths.stack("boucle");
        fs::create_dir(&looping.dir).unwrap();
        std::os::unix::fs::symlink("state.json", looping.state_file()).unwrap();
        let error = format!("{:#}", list(&paths).unwrap_err());
        assert!(error.contains("boucle/state.json"), "{error}");
        fs::remove_dir_all(&looping.dir).unwrap();

        // Sans droit d'entrer dans le dossier, comme un compte qui n'est pas root.
        let stack = paths.stack("demo");
        fs::set_permissions(&stack.dir, fs::Permissions::from_mode(0o000)).unwrap();
        let enters_anyway = fs::read_dir(&stack.dir).is_ok();
        let listed = list(&paths);
        fs::set_permissions(&stack.dir, fs::Permissions::from_mode(0o700)).unwrap();
        if enters_anyway {
            // Root entre partout : ce cas ne peut pas être essayé sous son compte.
            eprintln!(
                "listing_reports_a_stack_folder_it_cannot_enter : cas du dossier en 0000 sauté, l'essai tourne en root"
            );
        } else {
            let error = format!("{:#}", listed.unwrap_err());
            assert!(error.contains("demo/state.json"), "{error}");
        }
    }

    #[test]
    fn round_trip_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert!(list(&paths).unwrap().is_empty());
        let server = ServerUrl::parse("https://maj.example.fr").unwrap();
        let mut state = StackState::new("demo", server, Channel::Beta, "clé".into());
        state.installed = Some(ReleaseRecord::new(
            version("1.0.0"),
            vec!["maj.example.fr/demo/app@sha256:00".into()],
        ));
        state.reject(&version("1.2.0"));
        state.reject(&version("1.1.0"));
        state.reject(&version("1.2.0"));
        state.accept([
            "app|port|*:8080/tcp".to_owned(),
            "|external_volume|x".to_owned(),
        ]);
        state.accept(["app|port|*:8080/tcp".to_owned()]);
        state.save(&paths).unwrap();
        fs::create_dir_all(paths.state_dir.join("vide")).unwrap();

        let loaded = StackState::require(&paths, "demo").unwrap();
        assert_eq!(loaded, state);
        assert_eq!(loaded.rejected, [version("1.1.0"), version("1.2.0")]);
        assert_eq!(
            loaded.accepted_permissions,
            ["app|port|*:8080/tcp", "|external_volume|x"]
        );
        assert!(loaded.is_accepted("app|port|*:8080/tcp"));
        assert_eq!(list(&paths).unwrap(), ["demo"]);
        assert!(StackState::load(&paths, "autre").unwrap().is_none());
        let error = StackState::require(&paths, "autre")
            .unwrap_err()
            .to_string();
        assert!(error.contains("n'est pas installée"), "{error}");
    }
}
