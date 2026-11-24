//! Archive `stack.tar.gz` (docs/PROTOCOLE.md, « Archive `stack.tar.gz` ») : lecture contrôlée
//! et écriture reproductible, en mémoire.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};

use super::StackError;

pub const STACK_JSON: &str = "stack.json";
pub const COMPOSE_YAML: &str = "compose.yaml";
pub const ENV_EXAMPLE: &str = ".env.example";
pub const FILES_DIR: &str = "files";
pub const MAX_ENTRIES: usize = 1000;
/// Taille maximale du flux tar décompressé, en-têtes compris.
pub const MAX_TAR_BYTES: usize = 16 * 1024 * 1024;
/// Taille maximale de l'archive compressée : son flux tar fait `MAX_TAR_BYTES` au plus, et gzip
/// n'ajoute presque rien à des données qu'il ne sait pas réduire (1 Mio de marge). C'est la même
/// borne pour l'outil avant l'envoi, le serveur à la réception et l'agent au téléchargement : un
/// fichier plus gros ne peut pas être une archive de stack.
pub const MAX_ARCHIVE_BYTES: u64 = MAX_TAR_BYTES as u64 + (1 << 20);

/// Contenu d'une archive lue : ses fichiers, par chemin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackArchive {
    files: BTreeMap<String, Vec<u8>>,
}

impl StackArchive {
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }

    pub fn stack_json(&self) -> &[u8] {
        self.files
            .get(STACK_JSON)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn compose_yaml(&self) -> &[u8] {
        self.files
            .get(COMPOSE_YAML)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn env_example(&self) -> Option<&[u8]> {
        self.files.get(ENV_EXAMPLE).map(Vec::as_slice)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

/// Contrôle d'un chemin : composants ordinaires en UTF-8, et place permise dans une stack.
fn check_path(raw: &str, kind: Kind) -> Result<String, StackError> {
    let path = match kind {
        Kind::Dir => raw.strip_suffix('/').unwrap_or(raw),
        Kind::File => raw,
    };
    let components: Vec<&str> = path.split('/').collect();
    let bad_component = |c: &&str| {
        c.is_empty() || *c == "." || *c == ".." || c.starts_with("._") || c.contains('\\')
    };
    if path.is_empty() || components.iter().any(bad_component) {
        return Err(StackError::EntryPath(raw.to_owned()));
    }
    let allowed = matches!(
        (components.as_slice(), kind),
        ([STACK_JSON | COMPOSE_YAML | ENV_EXAMPLE], Kind::File)
            | ([FILES_DIR], Kind::Dir)
            | ([FILES_DIR, _, ..], _)
    );
    if !allowed {
        return Err(StackError::EntryUnexpected(path.to_owned()));
    }
    Ok(path.to_owned())
}

/// Dossiers parents d'un chemin : `files/a/b` donne `files` et `files/a`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(move |(index, _)| &path[..index])
}

/// Suit les chemins déjà vus, pour refuser un doublon ou un fichier qui serait aussi un dossier.
#[derive(Default)]
struct Paths {
    files: BTreeSet<String>,
    /// Dossiers, qu'ils aient leur entrée ou qu'ils ne soient que parents d'une autre.
    dirs: BTreeSet<String>,
    /// Dossiers qui ont leur entrée.
    listed_dirs: BTreeSet<String>,
}

impl Paths {
    fn add(&mut self, path: &str, kind: Kind) -> Result<(), StackError> {
        let duplicate = || StackError::EntryDuplicate(path.to_owned());
        if self.files.contains(path) || ancestors(path).any(|parent| self.files.contains(parent)) {
            return Err(duplicate());
        }
        match kind {
            Kind::File => {
                if self.dirs.contains(path) {
                    return Err(duplicate());
                }
                self.files.insert(path.to_owned());
            }
            Kind::Dir => {
                if !self.listed_dirs.insert(path.to_owned()) {
                    return Err(duplicate());
                }
                self.dirs.insert(path.to_owned());
            }
        }
        for parent in ancestors(path) {
            self.dirs.insert(parent.to_owned());
        }
        Ok(())
    }
}

fn invalid(error: impl std::fmt::Display) -> StackError {
    StackError::ArchiveInvalid(error.to_string())
}

/// Lit et contrôle une archive `stack.tar.gz`.
pub fn read(gz: &[u8]) -> Result<StackArchive, StackError> {
    let mut tar_bytes = Vec::new();
    GzDecoder::new(gz)
        .take(MAX_TAR_BYTES as u64 + 1)
        .read_to_end(&mut tar_bytes)
        .map_err(invalid)?;
    if tar_bytes.len() > MAX_TAR_BYTES {
        return Err(StackError::ArchiveTooLarge);
    }

    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    let mut paths = Paths::default();
    let mut files = BTreeMap::new();
    let mut count = 0usize;
    for entry in archive.entries().map_err(invalid)? {
        let mut entry = entry.map_err(invalid)?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(StackError::ArchiveTooManyEntries);
        }
        let raw = String::from_utf8(entry.path_bytes().into_owned()).map_err(|error| {
            StackError::EntryPath(String::from_utf8_lossy(error.as_bytes()).into_owned())
        })?;
        let entry_type = entry.header().entry_type();
        let kind = if entry_type.is_dir() {
            Kind::Dir
        } else if matches!(
            entry_type,
            tar::EntryType::Regular | tar::EntryType::Continuous
        ) {
            Kind::File
        } else {
            return Err(StackError::EntryType(raw));
        };
        let path = check_path(&raw, kind)?;
        paths.add(&path, kind)?;
        if kind == Kind::File {
            let mut content = Vec::new();
            entry.read_to_end(&mut content).map_err(invalid)?;
            files.insert(path, content);
        }
    }
    for required in [STACK_JSON, COMPOSE_YAML] {
        if !files.contains_key(required) {
            return Err(StackError::EntryMissing(required.to_owned()));
        }
    }
    Ok(StackArchive { files })
}

/// Écrit une archive reproductible à partir de fichiers, par chemin : entrées dans l'ordre de
/// leurs chemins (un dossier finit par `/`), chaque dossier parent ayant la sienne ; ustar, dates
/// à 0, propriétaire et groupe 0 sans nom, droits 0755 et 0644 ; gzip niveau 9 sans nom ni date.
pub fn write(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, StackError> {
    let mut paths = Paths::default();
    let mut entries: BTreeMap<String, Option<&[u8]>> = BTreeMap::new();
    for (path, content) in files {
        let path = check_path(path, Kind::File)?;
        paths.add(&path, Kind::File)?;
        for parent in ancestors(&path) {
            entries.insert(format!("{parent}/"), None);
        }
        entries.insert(path, Some(content.as_slice()));
    }
    for required in [STACK_JSON, COMPOSE_YAML] {
        if !files.contains_key(required) {
            return Err(StackError::EntryMissing(required.to_owned()));
        }
    }
    if entries.len() > MAX_ENTRIES {
        return Err(StackError::ArchiveTooManyEntries);
    }

    let mut builder = tar::Builder::new(Vec::new());
    for (name, content) in &entries {
        let mut header = tar::Header::new_ustar();
        header
            .set_path(name)
            .map_err(|_| StackError::EntryPath(name.clone()))?;
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        match content {
            None => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
            }
            Some(data) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(data.len() as u64);
            }
        }
        header.set_cksum();
        builder
            .append(&header, content.unwrap_or_default())
            .map_err(invalid)?;
    }
    let tar_bytes = builder.into_inner().map_err(invalid)?;
    if tar_bytes.len() > MAX_TAR_BYTES {
        return Err(StackError::ArchiveTooLarge);
    }

    let mut encoder = GzBuilder::new().write(Vec::new(), Compression::best());
    encoder.write_all(&tar_bytes).map_err(invalid)?;
    encoder.finish().map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (STACK_JSON.to_owned(), b"{}".to_vec()),
            (COMPOSE_YAML.to_owned(), b"services: {}\n".to_vec()),
            (ENV_EXAMPLE.to_owned(), b"PORT=8123\n".to_vec()),
            ("files/nginx/site.conf".to_owned(), b"server {}\n".to_vec()),
        ])
    }

    #[test]
    fn write_then_read() {
        let bytes = write(&sample()).unwrap();
        assert_eq!(write(&sample()).unwrap(), bytes, "reproductible");
        let archive = read(&bytes).unwrap();
        assert_eq!(archive.files(), &sample());
        assert_eq!(archive.env_example(), Some(&b"PORT=8123\n"[..]));

        let mut tar_bytes = Vec::new();
        GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut tar_bytes)
            .unwrap();
        let names: Vec<String> = tar::Archive::new(tar_bytes.as_slice())
            .entries()
            .unwrap()
            .map(|entry| String::from_utf8(entry.unwrap().path_bytes().into_owned()).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                ".env.example",
                "compose.yaml",
                "files/",
                "files/nginx/",
                "files/nginx/site.conf",
                "stack.json"
            ]
        );
    }

    #[test]
    fn write_refuses_what_read_refuses() {
        let cases = [
            ("../evil", "entry_path"),
            ("files/._x", "entry_path"),
            ("README.md", "entry_unexpected"),
            ("files", "entry_unexpected"),
            ("stack.json/x", "entry_unexpected"),
        ];
        for (path, code) in cases {
            let mut files = sample();
            files.insert(path.to_owned(), Vec::new());
            assert_eq!(write(&files).unwrap_err().code(), code, "{path}");
        }
        let mut files = sample();
        files.remove(COMPOSE_YAML);
        assert_eq!(write(&files).unwrap_err().code(), "entry_missing");
    }

    #[test]
    fn paths_conflict() {
        let mut paths = Paths::default();
        paths.add("files/a", Kind::File).unwrap();
        assert_eq!(
            paths.add("files/a/b", Kind::File).unwrap_err().code(),
            "entry_duplicate"
        );
        assert_eq!(
            paths.add("files/a", Kind::Dir).unwrap_err().code(),
            "entry_duplicate"
        );
        paths.add("files/c/d", Kind::File).unwrap();
        paths.add("files/c", Kind::Dir).unwrap();
        assert_eq!(
            paths.add("files/c", Kind::Dir).unwrap_err().code(),
            "entry_duplicate"
        );
        assert_eq!(
            paths.add("files/c", Kind::File).unwrap_err().code(),
            "entry_duplicate"
        );
    }
}
