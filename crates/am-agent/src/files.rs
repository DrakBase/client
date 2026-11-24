//! Écriture des fichiers de l'agent (jamais de fichier à moitié écrit à sa place finale),
//! verrou d'une stack et extraction d'une archive contrôlée.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::control;
use crate::paths::StackPaths;

/// Remplace `target` d'un seul coup : écriture à côté, synchronisation, puis renommage.
pub fn replace(target: &Path, contents: &[u8], mode: u32) -> Result<()> {
    replace_via(&sibling(target, ".tmp"), target, contents, mode)
}

/// Comme [`replace`], avec un fichier temporaire choisi par l'appelant, sur le même volume : pour
/// une cible dont le dossier n'est pas sûr (`control/`, où l'app a pu écrire), on le prend dans
/// un dossier qui l'est. Le fichier temporaire est toujours créé, jamais rouvert : un lien posé à
/// sa place n'est pas suivi.
pub fn replace_via(temp: &Path, target: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let written = (|| -> io::Result<()> {
        remove_entry(temp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(temp)?;
        file.set_permissions(Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(temp, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(temp);
    }
    written.with_context(|| format!("écriture de {}", target.display()))
}

/// Retire une entrée sans jamais suivre de lien : un lien est retiré lui-même, sa cible reste ;
/// un dossier part avec son contenu. Rien à faire si elle n'existe pas.
pub fn remove_entry(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Ouvre un dossier sans suivre de lien sur son nom : un lien posé à sa place fait échouer. Ce
/// qu'on fait ensuite sur le dossier ouvert (ses droits, par exemple) ne touche donc que lui.
pub fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

/// `.<nom><suffixe>` dans le même dossier que `path`, sur le même volume.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}{suffix}"))
}

/// Crée un dossier et ses parents, en 0755.
pub fn create_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .with_context(|| format!("création de {}", path.display()))
}

/// Le dossier existe-t-il, ouvert à d'autres comptes que root ? Un lien posé à sa place n'est pas
/// un dossier : [`private_dir`] le refusera.
pub fn open_to_others(dir: &Path) -> Result<bool> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            Ok(metadata.file_type().is_dir() && metadata.permissions().mode() & 0o077 != 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("droits de {}", dir.display())),
    }
}

/// Crée le dossier d'une stack en 0700, ou l'y remet : seul root y entre. Le dossier des
/// demandes, où l'app écrit en 0777, n'est ainsi pas à la portée des autres comptes de la machine.
/// Les droits sont réglés par le descripteur du dossier, ouvert sans suivre de lien. Rend `true`
/// si le dossier était ouvert à d'autres comptes que root.
pub fn private_dir(dir: &Path) -> Result<bool> {
    if let Some(parent) = dir.parent() {
        create_dir(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            return Err(error).with_context(|| format!("création de {}", dir.display()));
        }
        _ => {}
    }
    let handle = open_dir(dir).with_context(|| {
        format!(
            "ouverture du dossier {} (sans suivre de lien)",
            dir.display()
        )
    })?;
    (|| -> io::Result<bool> {
        let mode = handle.metadata()?.permissions().mode() & 0o7777;
        if mode != 0o700 {
            handle.set_permissions(Permissions::from_mode(0o700))?;
        }
        Ok(mode & 0o077 != 0)
    })()
    .with_context(|| format!("droits de {}", dir.display()))
}

/// Suffixe du nom sous lequel [`replace_dir`] met de côté le dossier qu'il remplace.
const REPLACED_SUFFIX: &str = ".old";

/// Met le dossier `source` à la place de `target`, sur le même volume. Un `target` déjà là est
/// mis de côté le temps du renommage, puis retiré ; si le renommage échoue, il revient. Un
/// processus arrêté entre les deux renommages laisse `target` de côté : [`settle_replaced`] le
/// remet.
pub fn replace_dir(source: &Path, target: &Path) -> Result<()> {
    let aside = sibling(target, REPLACED_SUFFIX);
    remove_dir(&aside)?;
    let had_target = match fs::rename(target, &aside) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("remplacement de {}", target.display()));
        }
    };
    if let Err(error) = fs::rename(source, target) {
        if had_target {
            let _ = fs::rename(&aside, target);
        }
        return Err(error).with_context(|| format!("mise en place de {}", target.display()));
    }
    if had_target {
        let _ = remove_dir(&aside);
    }
    Ok(())
}

/// Rattrape, dans `parent`, un [`replace_dir`] interrompu : un dossier mis de côté dont la place
/// est restée vide y revient ; si son remplaçant est en place, il part.
pub fn settle_replaced(parent: &Path) -> Result<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("lecture de {}", parent.display()));
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(replaced) = name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(REPLACED_SUFFIX))
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let aside = entry.path();
        let target = parent.join(replaced);
        if fs::symlink_metadata(&target).is_ok() {
            remove_dir(&aside)?;
        } else {
            fs::rename(&aside, &target)
                .with_context(|| format!("remise en place de {}", target.display()))?;
        }
    }
    Ok(())
}

/// Retire un dossier s'il existe.
pub fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("suppression de {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Retire un fichier s'il existe.
pub fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("suppression de {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Verrou d'une stack, tenu tant que le fichier rendu reste ouvert : une seule opération à la
/// fois par stack (docs/PROTOCOLE.md). Une autre opération en cours fait échouer tout de suite.
/// Le dossier de la stack est créé en 0700, ou y est remis ([`private_dir`]). Une fois le verrou
/// pris, le dossier d'une version qu'un remplacement interrompu a laissé de côté revient
/// ([`settle_replaced`]), avant que la commande ne lise `releases/`.
pub fn lock(path: &Path, app: &str) -> Result<File> {
    let file = try_lock(open_stack_lock(path)?, path, app)?;
    settle_releases(path)?;
    Ok(file)
}

/// Comme [`lock`], mais attend que l'autre opération finisse : pour les demandes de l'app, qui
/// ne doivent pas se perdre.
pub fn lock_wait(path: &Path, app: &str) -> Result<File> {
    let file = open_stack_lock(path)?;
    file.lock()
        .with_context(|| format!("verrou de {app} ({})", path.display()))?;
    settle_releases(path)?;
    Ok(file)
}

/// `releases/` de la stack dont `lock_file` est le verrou : voir [`settle_replaced`].
fn settle_releases(lock_file: &Path) -> Result<()> {
    match lock_file.parent() {
        Some(dir) => settle_replaced(
            &StackPaths {
                dir: dir.to_path_buf(),
            }
            .releases(),
        ),
        None => Ok(()),
    }
}

/// Verrou de l'agent lui-même (`self-update`), hors de toute stack : son dossier, celui des
/// stacks, reste en 0755.
pub fn lock_agent(path: &Path, what: &str) -> Result<File> {
    if let Some(dir) = path.parent() {
        create_dir(dir)?;
    }
    try_lock(open_lock(path)?, path, what)
}

fn try_lock(file: File, path: &Path, what: &str) -> Result<File> {
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(anyhow!(
            "une autre opération sur {what} est en cours (verrou {}) : réessayez quand elle sera finie",
            path.display()
        )),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("verrou {}", path.display()))
        }
    }
}

/// Ouvre le fichier de verrou d'une stack, dans son dossier, créé ou refermé en 0700.
fn open_stack_lock(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        control::close_stack_dir(dir)?;
    }
    open_lock(path)
}

/// Ouvre le fichier de verrou, en le créant s'il le faut.
fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("ouverture du verrou {}", path.display()))
}

/// Écrit les fichiers d'une archive déjà contrôlée (`am_protocol::stack::archive::read`) dans
/// `dir`, qui ne doit pas exister : dossiers en 0755, fichiers en 0644, quels que soient les
/// droits notés dans l'archive.
pub fn extract(files: &BTreeMap<String, Vec<u8>>, dir: &Path) -> Result<()> {
    DirBuilder::new()
        .mode(0o755)
        .create(dir)
        .with_context(|| format!("création de {}", dir.display()))?;
    for (path, content) in files {
        // Chemins relatifs faits de composants ordinaires : archive::read les a contrôlés.
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            create_dir(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&target)
            .with_context(|| format!("écriture de {}", target.display()))?;
        file.set_permissions(Permissions::from_mode(0o644))?;
        file.write_all(content)
            .with_context(|| format!("écriture de {}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_writes_with_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        replace(&target, b"un", 0o644).unwrap();
        replace(&target, b"deux", 0o600).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"deux");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!sibling(&target, ".tmp").exists());
    }

    #[test]
    fn replace_never_writes_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victime.txt");
        fs::write(&victim, "intact").unwrap();
        let target = dir.path().join("status.json");
        // Un lien à la place du fichier temporaire, et un autre à la place de la cible.
        std::os::unix::fs::symlink(&victim, sibling(&target, ".tmp")).unwrap();
        std::os::unix::fs::symlink(&victim, &target).unwrap();
        replace(&target, b"nouveau", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "intact");
        assert!(fs::symlink_metadata(&target).unwrap().file_type().is_file());
        assert_eq!(fs::read(&target).unwrap(), b"nouveau");
    }

    #[test]
    fn remove_entry_never_follows_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("ailleurs");
        fs::create_dir_all(kept.join("sous")).unwrap();
        fs::write(kept.join("sous/fichier"), "x").unwrap();

        let link = dir.path().join("lien");
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        remove_entry(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(kept.join("sous/fichier").exists());

        let tree = dir.path().join("arbre");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        std::os::unix::fs::symlink(&kept, tree.join("a/lien")).unwrap();
        remove_entry(&tree).unwrap();
        assert!(!tree.exists());
        assert!(kept.join("sous/fichier").exists());

        remove_entry(&dir.path().join("absent")).unwrap();
    }

    #[test]
    fn open_dir_refuses_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("vrai");
        fs::create_dir(&real).unwrap();
        open_dir(&real).unwrap();
        let link = dir.path().join("lien");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_dir(&link).is_err());
        let file = dir.path().join("fichier");
        fs::write(&file, "x").unwrap();
        assert!(open_dir(&file).is_err());
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo/.lock");
        let held = lock(&path, "demo").unwrap();
        let error = lock(&path, "demo").unwrap_err().to_string();
        assert!(error.contains("une autre opération sur demo"), "{error}");
        drop(held);
        lock(&path, "demo").unwrap();
    }

    #[test]
    fn the_stack_lock_keeps_its_folder_private() {
        let dir = tempfile::tempdir().unwrap();
        let stack = dir.path().join("lib/demo");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        drop(lock(&stack.join(".lock"), "demo").unwrap());
        assert_eq!(mode(&stack), 0o700);
        // Le dossier des stacks, lui, reste ouvert en lecture.
        assert_eq!(mode(&dir.path().join("lib")), 0o755);

        // Un dossier laissé ouvert par un agent plus ancien est refermé, par les deux verrous.
        for take in [lock, lock_wait] {
            fs::set_permissions(&stack, Permissions::from_mode(0o755)).unwrap();
            drop(take(&stack.join(".lock"), "demo").unwrap());
            assert_eq!(mode(&stack), 0o700);
        }

        // Le verrou de l'agent ne touche pas aux droits de son dossier.
        drop(lock_agent(&dir.path().join("lib/.self-update.lock"), "drakbase-stack").unwrap());
        assert_eq!(mode(&dir.path().join("lib")), 0o755);
    }

    #[test]
    fn a_folder_is_open_to_others_only_when_it_is_a_folder_they_can_enter() {
        let dir = tempfile::tempdir().unwrap();
        let stack = dir.path().join("demo");
        assert!(!open_to_others(&stack).unwrap());
        fs::create_dir(&stack).unwrap();
        fs::set_permissions(&stack, Permissions::from_mode(0o755)).unwrap();
        assert!(open_to_others(&stack).unwrap());
        fs::set_permissions(&stack, Permissions::from_mode(0o700)).unwrap();
        assert!(!open_to_others(&stack).unwrap());
        // Un lien vers un dossier ouvert n'est pas un dossier ouvert.
        fs::set_permissions(&stack, Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("lien");
        std::os::unix::fs::symlink(&stack, &link).unwrap();
        assert!(!open_to_others(&link).unwrap());
    }

    #[test]
    fn private_dir_refuses_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("ailleurs");
        fs::create_dir(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("demo");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(private_dir(&link).is_err());
        // Les droits de la cible n'ont pas changé.
        let mode = fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn replace_dir_swaps_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join(".1.1.0.next");
        let target = dir.path().join("1.1.0");
        fs::create_dir_all(source.join("files")).unwrap();
        fs::write(source.join("files/a"), "nouveau").unwrap();

        // Sans dossier en place.
        replace_dir(&source, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("files/a")).unwrap(),
            "nouveau"
        );
        assert!(!source.exists());

        // Avec un dossier en place : il part, sans rien laisser à côté.
        fs::create_dir(&source).unwrap();
        fs::write(source.join("b"), "suivant").unwrap();
        replace_dir(&source, &target).unwrap();
        assert_eq!(fs::read_to_string(target.join("b")).unwrap(), "suivant");
        assert!(!target.join("files").exists());
        assert!(!sibling(&target, ".old").exists());

        // Sans source, le dossier en place revient tel qu'il était.
        assert!(replace_dir(&source, &target).is_err());
        assert_eq!(fs::read_to_string(target.join("b")).unwrap(), "suivant");
        assert!(!sibling(&target, ".old").exists());
    }

    #[test]
    fn an_interrupted_replacement_is_settled_under_the_stack_lock() {
        let dir = tempfile::tempdir().unwrap();
        let stack = dir.path().join("demo");
        let releases = stack.join("releases");
        let aside = releases.join(".1.1.0.old");
        let target = releases.join("1.1.0");

        // Arrêté entre les deux renommages : le dossier mis de côté revient.
        fs::create_dir_all(&aside).unwrap();
        fs::write(aside.join("stack.json"), "ancien").unwrap();
        fs::create_dir(releases.join(".1.1.0.next")).unwrap();
        drop(lock(&stack.join(".lock"), "demo").unwrap());
        assert_eq!(
            fs::read_to_string(target.join("stack.json")).unwrap(),
            "ancien"
        );
        assert!(!aside.exists());
        // Le dossier examiné, lui, n'est pas une version en place : il reste où il est.
        assert!(releases.join(".1.1.0.next").is_dir());

        // Arrêté après le second : le remplaçant est en place, le dossier mis de côté part.
        fs::create_dir(&aside).unwrap();
        fs::write(aside.join("stack.json"), "plus ancien").unwrap();
        drop(lock_wait(&stack.join(".lock"), "demo").unwrap());
        assert_eq!(
            fs::read_to_string(target.join("stack.json")).unwrap(),
            "ancien"
        );
        assert!(!aside.exists());

        // Sans dossier `releases`, rien à faire.
        settle_replaced(&dir.path().join("absent")).unwrap();
    }

    #[test]
    fn extract_sets_modes() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("compose.yaml".to_owned(), b"services: {}\n".to_vec());
        files.insert("files/a/b.conf".to_owned(), b"x".to_vec());
        let target = dir.path().join("1.0.0");
        extract(&files, &target).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target.join("files/a/b.conf")), 0o644);
        assert_eq!(mode(&target.join("files/a")), 0o755);
        assert_eq!(
            fs::read(target.join("compose.yaml")).unwrap(),
            b"services: {}\n"
        );
        // Le dossier de la version ne doit pas exister déjà.
        assert!(extract(&files, &target).is_err());
    }
}
