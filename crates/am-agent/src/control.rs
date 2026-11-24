//! Dossier de contrôle des mises à jour validées dans l'app (docs/PROTOCOLE.md) : `status.json`,
//! écrit par l'agent ; `request/`, où l'app dépose ses demandes ; et le fichier Compose de
//! l'agent, qui monte ce dossier dans le service de l'app.
//!
//! Seule une stack dont la version a `app_control` a ce dossier : il est créé avec le fichier
//! Compose de l'agent, et retiré quand la version qui tourne n'en veut plus. Le dossier de la
//! stack, en 0700, le tient hors de portée des autres comptes de la machine.
//!
//! L'agent tourne en root, et l'app écrit dans `request/`. Une stack installée par un agent plus
//! ancien a même tout `control/` monté en écriture, jusqu'à sa prochaine mise à jour ou relance.
//! Rien de ce qui s'y trouve n'est donc tenu pour sûr : aucun lien n'est suivi, et seuls des
//! fichiers ordinaires sont lus.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use am_protocol::stack::StackError;
use am_protocol::stack::control::{ENV_CONTROL, MAX_REQUEST_BYTES, REQUEST_DIR, Request};
use am_protocol::stack::manifest::StackManifest;
use anyhow::{Context, Result};
use serde_json::json;

use crate::files;
use crate::paths::StackPaths;
use crate::state::StackState;

/// Une demande en cours d'écriture (nom en `.…`) plus vieille que ceci est abandonnée.
const STALE_TEMP: Duration = Duration::from_secs(60);

/// Crée le dossier de contrôle (0755, à root) et celui des demandes (0777 : l'app y écrit sous
/// l'utilisateur de son conteneur).
pub fn ensure_dirs(stack: &StackPaths) -> Result<()> {
    let control = stack.control_dir();
    files::create_dir(&control)?;
    set_mode(&control, 0o755).with_context(|| format!("droits de {}", control.display()))?;
    let requests = stack.request_dir();
    ensure_request_dir(&requests)
        .with_context(|| format!("dossier des demandes {}", requests.display()))
}

/// Règle les droits d'un dossier par son descripteur, ouvert sans suivre de lien : jamais par
/// son nom, qu'un lien a pu prendre.
fn set_mode(dir: &Path, mode: u32) -> io::Result<()> {
    files::open_dir(dir)?.set_permissions(Permissions::from_mode(mode))
}

/// `request` doit être un vrai dossier. Autre chose (un lien vers un dossier de la machine, par
/// exemple) est retiré sans que sa cible soit touchée, puis le dossier est recréé.
fn ensure_request_dir(requests: &Path) -> io::Result<()> {
    let is_dir = match fs::symlink_metadata(requests) {
        Ok(metadata) => metadata.file_type().is_dir(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !is_dir {
        files::remove_entry(requests)?;
        // Sans `recursive` : la création échoue si un lien a repris la place entre-temps.
        DirBuilder::new().mode(0o777).create(requests)?;
    }
    set_mode(requests, 0o777)
}

/// Referme le dossier d'une stack en 0700 ([`files::private_dir`]). S'il était ouvert à d'autres
/// comptes, ce qui attend dans `request/` part sans être lu : un compte de la machine a pu y
/// déposer une demande, que rien ne distingue de celles de l'app.
pub fn close_stack_dir(dir: &Path) -> Result<()> {
    let stack = StackPaths {
        dir: dir.to_path_buf(),
    };
    // Vidé une première fois tant qu'il est ouvert : un agent arrêté avant de l'avoir refermé le
    // retrouve ouvert, et recommence.
    if files::open_to_others(dir)? {
        discard_requests(&stack)?;
    }
    if files::private_dir(dir)? {
        // Ce qui a pu arriver pendant que le dossier se refermait.
        discard_requests(&stack)?;
    }
    Ok(())
}

/// Retire tout ce qui attend dans `request/`, sans rien lire ni suivre de lien : un lien ou un
/// fichier posé à la place du dossier est retiré lui-même.
pub fn discard_requests(stack: &StackPaths) -> Result<()> {
    let dir = stack.request_dir();
    (|| -> io::Result<()> {
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return files::remove_entry(&dir),
            // Pas de dossier des demandes, ni de dossier de contrôle.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        let _pinned = Pinned::enter(&dir)?;
        for entry in fs::read_dir(".")? {
            files::remove_entry(Path::new(&entry?.file_name()))?;
        }
        Ok(())
    })()
    .with_context(|| format!("dossier des demandes {}", dir.display()))
}

/// La stack a-t-elle un dossier de contrôle ? Un vrai dossier, pas un lien.
pub fn exists(stack: &StackPaths) -> bool {
    fs::symlink_metadata(stack.control_dir()).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Retire le dossier de contrôle d'une stack dont la version qui tourne n'a pas `app_control`.
pub fn remove(stack: &StackPaths) -> Result<()> {
    let control = stack.control_dir();
    files::remove_entry(&control).with_context(|| format!("suppression de {}", control.display()))
}

/// Écrit `status.json` à partir de l'état de la stack, si elle a un dossier de contrôle : sans
/// lui, aucune app ne lit cet état, et rien n'est créé.
pub fn write_status(stack: &StackPaths, state: &StackState) -> Result<()> {
    if !exists(stack) {
        return Ok(());
    }
    let target = stack.status_file();
    // Un dossier posé à la place du fichier ferait échouer le renommage.
    if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        files::remove_entry(&target)
            .with_context(|| format!("suppression de {}", target.display()))?;
    }
    // Préparé dans le dossier de la stack, où l'app n'écrit pas, puis renommé : rien n'est ouvert
    // dans `control/`, et le renommage remplace un lien sans toucher à sa cible.
    files::replace_via(
        &stack.status_temp(),
        &target,
        state.status().to_json().as_bytes(),
        0o644,
    )
}

/// Écrit, dans le dossier d'une version, le fichier Compose qui monte le dossier de contrôle au
/// chemin `app_control` du service et donne `DRAKBASE_CONTROL` à son environnement. Le dossier
/// est monté en lecture seule, et `request/` par-dessus, en écriture : l'app n'écrit que là, même
/// en root dans son conteneur. Sans `app_control`, il n'y a pas de fichier.
pub fn write_compose(
    stack: &StackPaths,
    release_dir: &Path,
    manifest: &StackManifest,
) -> Result<()> {
    let file = StackPaths::control_compose_file(release_dir);
    let Some(target) = &manifest.app_control else {
        return files::remove_file(&file);
    };
    ensure_dirs(stack)?;
    let document = json!({
        "services": {
            manifest.service.as_str(): {
                "environment": { ENV_CONTROL: target },
                "volumes": [
                    {
                        "type": "bind",
                        "source": stack.control_dir().to_string_lossy(),
                        "target": target,
                        "read_only": true,
                    },
                    {
                        "type": "bind",
                        "source": stack.request_dir().to_string_lossy(),
                        "target": format!("{target}/{REQUEST_DIR}"),
                    },
                ],
            },
        },
    });
    let mut text = serde_json::to_string_pretty(&document)?;
    text.push('\n');
    files::replace(&file, text.as_bytes(), 0o644)
}

/// Une demande lue dans `request/`.
pub struct Received {
    pub name: String,
    pub request: Result<Request, StackError>,
}

/// Lit et retire les demandes de l'app, dans l'ordre de leurs noms. Une demande illisible est
/// retirée aussi, pour ne pas relancer l'agent sans fin. Une demande encore en cours d'écriture
/// (nom en `.…`) est attendue quelques secondes ; elle est retirée si elle traîne, ou si sa date
/// ne dit pas depuis quand (illisible, ou dans le futur).
pub fn take_requests(stack: &StackPaths) -> Result<Vec<Received>> {
    ensure_dirs(stack)?;
    let dir = stack.request_dir();
    let _pinned =
        Pinned::enter(&dir).with_context(|| format!("dossier des demandes {}", dir.display()))?;
    take_from_current_dir().with_context(|| format!("lecture de {}", dir.display()))
}

/// Le dossier courant du processus, placé dans un dossier contrôlé, et rendu à la fin.
///
/// Les chemins relatifs désignent alors ce dossier-là, quoi que devienne son nom : si l'app le
/// donne ensuite à un lien, l'agent ne le suit pas. La bibliothèque standard n'a pas d'autre
/// moyen d'ouvrir et de supprimer « dans ce dossier » plutôt que « sous ce nom ».
struct Pinned {
    previous: PathBuf,
}

impl Pinned {
    /// Entre dans `dir`, puis contrôle que le dossier où l'on se trouve est bien celui, véritable,
    /// que porte ce nom : entrer dans un lien, c'est se trouver ailleurs, et c'est refusé.
    fn enter(dir: &Path) -> io::Result<Self> {
        let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        env::set_current_dir(dir)?;
        let pinned = Self { previous };
        let here = fs::metadata(".")?;
        let named = fs::symlink_metadata(dir)?;
        if !named.file_type().is_dir() || (here.dev(), here.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::other(
                "ce n'est pas un vrai dossier (un lien a pris sa place)",
            ));
        }
        Ok(pinned)
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

/// Les demandes du dossier courant, qui est `request/` (voir [`Pinned`]).
fn take_from_current_dir() -> io::Result<Vec<Received>> {
    for attempt in 0..5 {
        let mut names: Vec<OsString> = Vec::new();
        let mut writing = false;
        for entry in fs::read_dir(".")?.flatten() {
            let name = entry.file_name();
            if name.as_encoded_bytes().starts_with(b".") {
                // `DirEntry::metadata` ne suit pas les liens.
                let metadata = match entry.metadata() {
                    // Renommée depuis la lecture du dossier : la demande est prête, et le tour
                    // suivant la lit.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        writing = true;
                        continue;
                    }
                    metadata => metadata,
                };
                // Sans âge lisible (date illisible, ou dans le futur), le fichier ne serait
                // jamais périmé, et le dossier jamais vide : il est retiré tout de suite.
                let stale = metadata
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_none_or(|age| age > STALE_TEMP);
                if stale {
                    let _ = files::remove_entry(Path::new(&name));
                } else {
                    writing = true;
                }
            } else {
                names.push(name);
            }
        }
        if !names.is_empty() || !writing || attempt == 4 {
            names.sort();
            let received = names
                .iter()
                .map(|name| {
                    let request = read_request(name);
                    // Le nom tel qu'il est sur le disque, sans suivre de lien : un lien part, sa
                    // cible reste.
                    let _ = files::remove_entry(Path::new(name));
                    Received {
                        name: name.to_string_lossy().into_owned(),
                        request,
                    }
                })
                .collect();
            return Ok(received);
        }
        thread::sleep(Duration::from_secs(1));
    }
    Ok(Vec::new())
}

/// Lit une demande du dossier courant : un fichier `.json` ordinaire de `MAX_REQUEST_BYTES` au
/// plus. Un lien, un dossier, un tube ou un périphérique n'est pas ouvert.
fn read_request(name: &OsStr) -> Result<Request, StackError> {
    let shown = name.to_string_lossy();
    let invalid = |what: String| StackError::RequestInvalid(what);
    if !name.as_encoded_bytes().ends_with(b".json") {
        return Err(invalid(format!("« {shown} » n'est pas un fichier .json")));
    }
    let not_regular = || invalid(format!("« {shown} » n'est pas un fichier ordinaire"));
    let listed = fs::symlink_metadata(name).map_err(|error| invalid(error.to_string()))?;
    if !listed.file_type().is_file() {
        return Err(not_regular());
    }
    if listed.len() > MAX_REQUEST_BYTES as u64 {
        return Err(StackError::RequestTooLarge);
    }
    // Sans suivre de lien ni attendre l'autre bout d'un tube : si l'entrée a changé depuis le
    // contrôle ci-dessus, l'ouverture échoue, ou le contrôle du fichier ouvert la refuse.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(name)
        .map_err(|error| invalid(error.to_string()))?;
    let opened = file
        .metadata()
        .map_err(|error| invalid(error.to_string()))?;
    if !opened.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(error.to_string()))?;
    Request::parse(&bytes)
}
