//! Écriture des fichiers produits par l'outil : jamais de fichier à moitié écrit à sa place finale.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::TempDir;

/// `path` suivi de `suffix` : `a.app.tar.gz` et `.sig` donnent `a.app.tar.gz.sig`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(suffix);
    name.into()
}

/// Crée `path` avec les droits `mode`, sans jamais remplacer un fichier existant. Si l'écriture
/// échoue après la création, le fichier est retiré.
pub fn create_new(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

/// Remplace `target` d'un seul coup par un fichier lisible par tous (0644).
pub fn replace(target: &Path, contents: &[u8]) -> Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(".appsmarket-")
        .tempfile_in(parent(target))
        .with_context(|| format!("écriture de {}", target.display()))?;
    temp.write_all(contents)?;
    temp.as_file()
        .set_permissions(Permissions::from_mode(0o644))?;
    temp.as_file().sync_all()?;
    temp.persist(target)
        .map_err(|error| error.error)
        .with_context(|| format!("écriture de {}", target.display()))?;
    Ok(())
}

/// Remplace `target` d'un seul coup par un fichier lisible par toi seul (0600, comme tout fichier
/// de tempfile).
pub fn replace_private(target: &Path, contents: &[u8]) -> Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(".appsmarket-")
        .tempfile_in(parent(target))
        .with_context(|| format!("écriture de {}", target.display()))?;
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    temp.persist(target)
        .map_err(|error| error.error)
        .with_context(|| format!("écriture de {}", target.display()))?;
    Ok(())
}

/// Lit un fichier JSON du dossier de l'outil ; `None` s'il n'existe pas. `hint` complète l'erreur
/// d'un fichier illisible par ce qu'il faut en faire : «  : refais « drakbase login » ».
pub fn read_json<T: DeserializeOwned>(path: &Path, hint: &str) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("lecture de {}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("{} illisible{hint}", path.display()))
}

/// Écrit un fichier JSON du dossier de l'outil, d'un seul coup, lisible par toi seul (0600) ; son
/// dossier est créé en 0700 s'il manque.
pub fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = parent(path);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("création de {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    replace_private(path, text.as_bytes())
}

/// Dossier de travail temporaire à côté de `target`, sur le même volume, pour y préparer un
/// fichier puis le mettre en place avec [`move_into_place`].
pub fn work_dir_beside(target: &Path) -> Result<TempDir> {
    let dir = parent(target);
    tempfile::Builder::new()
        .prefix(".appsmarket-")
        .tempdir_in(dir)
        .with_context(|| format!("création d'un dossier temporaire dans {}", dir.display()))
}

/// Met `from` à la place de `to`. Sans `replace`, refuse d'écraser un fichier existant.
pub fn move_into_place(from: &Path, to: &Path, replace: bool) -> Result<()> {
    if replace {
        fs::rename(from, to)
    } else {
        // Un lien physique échoue si `to` existe : la vérification et l'écriture ne font qu'un.
        fs::hard_link(from, to)
    }
    .with_context(|| format!("écriture de {}", to.display()))
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_is_appended_to_the_whole_name() {
        assert_eq!(
            with_suffix(Path::new("dist/A-1.0.0.app.tar.gz"), ".sig"),
            PathBuf::from("dist/A-1.0.0.app.tar.gz.sig")
        );
    }

    #[test]
    fn create_new_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.key");
        create_new(&path, b"un", 0o600).unwrap();
        let error = create_new(&path, b"deux", 0o600).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"un");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn move_into_place_respects_replace() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("neuf"), dir.path().join("final"));
        fs::write(&from, "neuf").unwrap();
        fs::write(&to, "ancien").unwrap();
        assert!(move_into_place(&from, &to, false).is_err());
        assert_eq!(fs::read_to_string(&to).unwrap(), "ancien");
        move_into_place(&from, &to, true).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "neuf");
    }

    #[test]
    fn replace_writes_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sig");
        fs::write(&path, "ancienne").unwrap();
        replace(&path, b"nouvelle").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "nouvelle");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
