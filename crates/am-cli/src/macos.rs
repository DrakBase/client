//! Ce que l'outil lit dans une app macOS : les clés utiles de son Info.plist et les processeurs de
//! son exécutable, dans le bundle lui-même ou dans son archive `.app.tar.gz`. Le serveur fait les
//! mêmes contrôles à l'envoi (docs/API.md, « Envoi d'un artefact ») ; les faire ici évite
//! d'envoyer pour rien une archive qu'il refusera.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::{Component, Path};

use am_protocol::signature::{KeyId, PublicKey};
use am_protocol::{Arch, Version};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use tar::EntryType;

use crate::project;

/// Début de l'exécutable lu pour ses processeurs : l'en-tête Mach-O, et la table d'un binaire
/// universel.
const HEADER_BYTES: u64 = 4096;
/// Plus gros Info.plist lu dans une archive.
const MAX_INFO_PLIST_BYTES: u64 = 1024 * 1024;
/// Au-delà, ce n'est pas un binaire universel mais, par exemple, une classe Java, qui commence
/// aussi par 0xCAFEBABE.
const MAX_FAT_ARCHS: usize = 32;

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// Clés de l'Info.plist dont l'outil se sert.
#[derive(Clone, Debug)]
pub struct BundleInfo {
    pub identifier: Option<String>,
    /// `CFBundleShortVersionString` tel qu'écrit.
    pub raw_version: String,
    /// La même version, sous sa forme canonique.
    pub version: Version,
    pub executable: Option<String>,
    pub minimum_macos: Option<String>,
    /// `DrakBasePublicKeys` (ou l'ancienne `AppsMarketPublicKeys`) : les clés publiques que l'app
    /// embarque, au format Tauri.
    pub public_keys: Vec<String>,
}

impl BundleInfo {
    /// Lit l'Info.plist ; `origin` le désigne dans les messages.
    pub fn from_plist(value: &plist::Value, origin: &str) -> Result<Self> {
        let info = value
            .as_dictionary()
            .with_context(|| format!("{origin} : dictionnaire attendu"))?;
        let text = |key: &str| {
            info.get(key)
                .and_then(plist::Value::as_string)
                .map(str::to_owned)
        };
        let raw_version = text("CFBundleShortVersionString")
            .with_context(|| format!("{origin} n'a pas de CFBundleShortVersionString"))?;
        let version = Version::parse_release(&raw_version)
            .context("CFBundleShortVersionString de l'Info.plist")?;
        let public_keys = match project::PUBLIC_KEYS_KEY.get(info) {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .and_then(|keys| {
                    keys.iter()
                        .map(|key| key.as_string().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .with_context(|| {
                    format!(
                        "{origin} : {} doit être un tableau de chaînes",
                        project::PUBLIC_KEYS_KEY.name
                    )
                })?,
        };
        Ok(Self {
            identifier: text("CFBundleIdentifier"),
            raw_version,
            version,
            executable: text("CFBundleExecutable"),
            minimum_macos: text("LSMinimumSystemVersion"),
            public_keys,
        })
    }

    /// Identifiants des clés embarquées, dans l'ordre de `DrakBasePublicKeys`.
    pub fn embedded_key_ids(&self) -> Result<Vec<KeyId>> {
        self.public_keys
            .iter()
            .map(|key| {
                PublicKey::from_tauri(key)
                    .map(|key| key.key_id())
                    .context("DrakBasePublicKeys : clé publique illisible (format Tauri attendu)")
            })
            .collect()
    }

    /// Nom de l'exécutable principal, qui doit être un simple nom de fichier.
    fn executable_name(&self, origin: &str) -> Result<&str> {
        let name = self
            .executable
            .as_deref()
            .with_context(|| format!("{origin} n'a pas de CFBundleExecutable"))?;
        ensure!(
            !name.is_empty() && !name.contains('/') && name != "..",
            "{origin} : CFBundleExecutable « {name} » n'est pas un nom de fichier"
        );
        Ok(name)
    }
}

/// Processeur d'une tranche Mach-O.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cpu {
    X86_64,
    /// arm64, arm64e et leurs variantes : le même type de processeur.
    Arm64,
    Other(u32),
}

impl fmt::Display for Cpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cpu::X86_64 => f.write_str("x86_64"),
            Cpu::Arm64 => f.write_str("arm64"),
            Cpu::Other(code) => write!(f, "processeur 0x{code:08x}"),
        }
    }
}

/// Processeurs distincts, triés : arm64, x86_64, puis les autres.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpus(Vec<Cpu>);

impl Cpus {
    pub fn new(mut cpus: Vec<Cpu>) -> Self {
        cpus.sort_by_key(|cpu| match cpu {
            Cpu::Arm64 => (0, 0),
            Cpu::X86_64 => (1, 0),
            Cpu::Other(code) => (2, *code),
        });
        cpus.dedup();
        Self(cpus)
    }

    fn has(&self, cpu: Cpu) -> bool {
        self.0.contains(&cpu)
    }

    /// L'arch d'artefact de ce binaire : `universal` s'il a arm64 et x86_64.
    pub fn arch(&self) -> Option<Arch> {
        match (self.has(Cpu::Arm64), self.has(Cpu::X86_64)) {
            (true, true) => Some(Arch::Universal),
            (true, false) => Some(Arch::Aarch64),
            (false, true) => Some(Arch::X86_64),
            (false, false) => None,
        }
    }

    /// Un artefact déclaré pour `arch` peut-il contenir ce binaire ?
    pub fn serve(&self, arch: Arch) -> bool {
        match arch {
            Arch::Universal => self.has(Cpu::Arm64) && self.has(Cpu::X86_64),
            Arch::Aarch64 => self.has(Cpu::Arm64),
            Arch::X86_64 => self.has(Cpu::X86_64),
            _ => false,
        }
    }
}

impl fmt::Display for Cpus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.0.iter().map(Cpu::to_string).collect();
        match names.as_slice() {
            [] => f.write_str("aucun processeur"),
            [one] => f.write_str(one),
            [rest @ .., last] => write!(f, "{} et {last}", rest.join(", ")),
        }
    }
}

/// Processeurs d'un exécutable Mach-O, d'après son début. `None` si ce n'est pas du Mach-O.
pub fn macho_cpus(header: &[u8]) -> Option<Cpus> {
    let word = |at: usize| -> Option<[u8; 4]> { header.get(at..at + 4)?.try_into().ok() };
    let magic = word(0)?;
    match magic {
        // Binaire simple : mach_header ou mach_header_64, petit-boutiste sur un Mac.
        [0xce | 0xcf, 0xfa, 0xed, 0xfe] => Some(Cpus::new(vec![cpu(u32::from_le_bytes(word(4)?))])),
        // Binaire universel : fat_header, puis des fat_arch (20 octets) ou des fat_arch_64 (32),
        // gros-boutistes.
        [0xca, 0xfe, 0xba, 0xbe | 0xbf] => {
            let entry = if magic[3] == 0xbe { 20 } else { 32 };
            let count = usize::try_from(u32::from_be_bytes(word(4)?)).ok()?;
            if count == 0 || count > MAX_FAT_ARCHS {
                return None;
            }
            let cpus = (0..count)
                .map(|index| word(8 + index * entry).map(|bytes| cpu(u32::from_be_bytes(bytes))))
                .collect::<Option<Vec<_>>>()?;
            Some(Cpus::new(cpus))
        }
        _ => None,
    }
}

fn cpu(code: u32) -> Cpu {
    match code {
        CPU_TYPE_X86_64 => Cpu::X86_64,
        CPU_TYPE_ARM64 => Cpu::Arm64,
        other => Cpu::Other(other),
    }
}

/// Processeurs de l'exécutable principal d'un bundle.
pub fn bundle_cpus(bundle: &Path, info: &BundleInfo) -> Result<Cpus> {
    let info_path = bundle.join("Contents/Info.plist");
    let name = info.executable_name(&info_path.display().to_string())?;
    let path = bundle.join("Contents/MacOS").join(name);
    let mut header = Vec::new();
    File::open(&path)
        .and_then(|file| file.take(HEADER_BYTES).read_to_end(&mut header))
        .with_context(|| format!("lecture de l'exécutable {}", path.display()))?;
    macho_cpus(&header)
        .with_context(|| format!("{} n'est pas un exécutable Mach-O", path.display()))
}

/// Ce que contient une archive `.app.tar.gz`, dont le seul dossier à la racine est `Nom.app`.
#[derive(Clone, Debug)]
pub struct ArchiveContents {
    pub info: BundleInfo,
    pub cpus: Cpus,
}

/// Parcourt l'archive une fois : un seul dossier `Nom.app` à sa racine, son Info.plist, et le début
/// des fichiers de `Contents/MacOS`, pour les processeurs de l'exécutable.
pub fn inspect_archive(path: &Path) -> Result<ArchiveContents> {
    let file = File::open(path).with_context(|| format!("ouverture de {}", path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::new(file)));
    let mut top: Option<String> = None;
    let mut info_plist: Option<Vec<u8>> = None;
    let mut executables: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let unreadable = "archive illisible : .app.tar.gz attendu (tar compressé par gzip)";
    for entry in archive.entries().context(unreadable)? {
        let entry = entry.context(unreadable)?;
        let entry_path = entry.path().context(unreadable)?.into_owned();
        let mut parts = Vec::new();
        for component in entry_path.components() {
            match component {
                Component::Normal(part) => parts.push(
                    part.to_str()
                        .with_context(|| format!("{} : nom illisible", entry_path.display()))?
                        .to_owned(),
                ),
                Component::CurDir => {}
                _ => bail!(
                    "l'archive contient un chemin interdit : {}",
                    entry_path.display()
                ),
            }
        }
        let Some(first) = parts.first() else {
            continue;
        };
        match &top {
            None => top = Some(first.clone()),
            Some(top) if top != first => bail!(
                "l'archive doit contenir un seul dossier Nom.app à sa racine, elle contient {top} et {first}"
            ),
            Some(_) => {}
        }
        let regular = entry.header().entry_type() == EntryType::Regular;
        match parts.as_slice() {
            [_, contents, name] if regular && contents == "Contents" && name == "Info.plist" => {
                let mut bytes = Vec::new();
                entry
                    .take(MAX_INFO_PLIST_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .context(unreadable)?;
                ensure!(
                    bytes.len() as u64 <= MAX_INFO_PLIST_BYTES,
                    "Info.plist de l'archive trop gros"
                );
                info_plist = Some(bytes);
            }
            [_, contents, macos, name] if regular && contents == "Contents" && macos == "MacOS" => {
                let mut bytes = Vec::new();
                entry
                    .take(HEADER_BYTES)
                    .read_to_end(&mut bytes)
                    .context(unreadable)?;
                executables.insert(name.clone(), bytes);
            }
            _ => {}
        }
    }

    let top = top.context("archive vide")?;
    ensure!(
        top.len() > ".app".len() && top.ends_with(".app"),
        "l'archive doit contenir un dossier Nom.app à sa racine, pas « {top} »"
    );
    let origin = format!("{top}/Contents/Info.plist");
    let bytes = info_plist.with_context(|| format!("l'archive n'a pas de {origin}"))?;
    let value = plist::Value::from_reader(Cursor::new(bytes))
        .with_context(|| format!("{origin} illisible"))?;
    let info = BundleInfo::from_plist(&value, &origin)?;
    let name = info.executable_name(&origin)?;
    let executable = format!("{top}/Contents/MacOS/{name}");
    let header = executables
        .get(name)
        .with_context(|| format!("l'archive n'a pas l'exécutable {executable}"))?;
    let cpus = macho_cpus(header)
        .with_context(|| format!("{executable} n'est pas un exécutable Mach-O"))?;
    Ok(ArchiveContents { info, cpus })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// En-tête Mach-O 64 bits simple, petit-boutiste.
    fn thin(cpu: u32) -> Vec<u8> {
        let mut bytes = vec![0xcf, 0xfa, 0xed, 0xfe];
        bytes.extend(cpu.to_le_bytes());
        bytes.extend([0; 24]);
        bytes
    }

    /// En-tête d'un binaire universel avec ces processeurs.
    fn fat(cpus: &[u32]) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        bytes.extend((cpus.len() as u32).to_be_bytes());
        for cpu in cpus {
            bytes.extend(cpu.to_be_bytes());
            bytes.extend([0; 16]);
        }
        bytes
    }

    #[test]
    fn mach_o_headers_give_the_processors() {
        let arm = macho_cpus(&thin(CPU_TYPE_ARM64)).unwrap();
        assert_eq!(arm.arch(), Some(Arch::Aarch64));
        assert_eq!(arm.to_string(), "arm64");
        let intel = macho_cpus(&thin(CPU_TYPE_X86_64)).unwrap();
        assert_eq!(intel.arch(), Some(Arch::X86_64));
        // /usr/bin/true sur macOS 27 : x86_64, arm64e et arm64e.x1, deux fois arm64 pour nous.
        let universal =
            macho_cpus(&fat(&[CPU_TYPE_X86_64, CPU_TYPE_ARM64, CPU_TYPE_ARM64])).unwrap();
        assert_eq!(universal.arch(), Some(Arch::Universal));
        assert_eq!(universal.to_string(), "arm64 et x86_64");
        assert!(universal.serve(Arch::Universal));
        assert!(universal.serve(Arch::Aarch64));
        assert!(!arm.serve(Arch::Universal));
        assert!(!arm.serve(Arch::X86_64));

        // 32 bits, classe Java, texte : pas un exécutable de Mac utilisable.
        assert_eq!(macho_cpus(&thin(7)).unwrap().arch(), None);
        let mut java = vec![0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x41];
        java.extend([0; 64]);
        assert_eq!(macho_cpus(&java), None);
        assert_eq!(macho_cpus(b"#!/bin/sh\n"), None);
        assert_eq!(macho_cpus(&[0xca, 0xfe]), None);
        // Une table tronquée n'est pas lue au-delà des octets fournis.
        assert_eq!(macho_cpus(&fat(&[CPU_TYPE_ARM64])[..10]), None);
    }

    #[test]
    fn the_real_true_command_is_universal() {
        let mut header = Vec::new();
        File::open("/usr/bin/true")
            .unwrap()
            .take(HEADER_BYTES)
            .read_to_end(&mut header)
            .unwrap();
        let cpus = macho_cpus(&header).unwrap();
        assert!(cpus.serve(Arch::Aarch64), "{cpus}");
    }

    fn plist(version: &str, keys: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Demo</string>
<key>CFBundleIdentifier</key><string>fr.example.Demo</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
{keys}
</dict></plist>
"#
        )
    }

    /// Archive gzip : chaque entrée est un chemin et son contenu.
    fn archive(path: &Path, entries: &[(&str, &[u8])]) {
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(path).unwrap(),
            Compression::fast(),
        ));
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(contents.len() as u64);
            header.set_entry_type(EntryType::Regular);
            builder.append_data(&mut header, name, *contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn archives_are_read_without_extracting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Demo-1.2.0.app.tar.gz");
        let key = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/keys/test-app.pub"),
        )
        .unwrap();
        let keys = format!("<key>DrakBasePublicKeys</key><array><string>{key}</string></array>");
        let info = plist("1.2", &keys);
        let executable = fat(&[CPU_TYPE_ARM64, CPU_TYPE_X86_64]);
        archive(
            &path,
            &[
                ("./Demo.app/Contents/Info.plist", info.as_bytes()),
                ("Demo.app/Contents/MacOS/Demo", &executable),
                ("Demo.app/Contents/MacOS/Assistant", &thin(CPU_TYPE_X86_64)),
                ("Demo.app/Contents/Resources/a.txt", b"texte"),
            ],
        );
        let contents = inspect_archive(&path).unwrap();
        assert_eq!(contents.info.identifier.as_deref(), Some("fr.example.Demo"));
        assert_eq!(contents.info.version.to_string(), "1.2.0");
        assert_eq!(contents.info.raw_version, "1.2");
        assert_eq!(contents.cpus.arch(), Some(Arch::Universal));
        let ids = contents.info.embedded_key_ids().unwrap();
        assert_eq!(
            ids,
            [PublicKey::from_tauri(&key).unwrap().key_id()],
            "{ids:?}"
        );

        let refused = |entries: &[(&str, &[u8])], message: &str| {
            archive(&path, entries);
            let error = format!("{:#}", inspect_archive(&path).unwrap_err());
            assert!(error.contains(message), "{error}");
        };
        let plain = plist("1.2.0", "");
        refused(
            &[
                ("Demo.app/Contents/Info.plist", plain.as_bytes()),
                ("Autre.app/Contents/MacOS/Demo", &executable),
            ],
            "un seul dossier",
        );
        refused(
            &[("Demo.app/Contents/MacOS/Demo", &executable)],
            "n'a pas de Demo.app/Contents/Info.plist",
        );
        refused(
            &[("Demo.app/Contents/Info.plist", plain.as_bytes())],
            "n'a pas l'exécutable",
        );
        refused(
            &[
                ("Demo.app/Contents/Info.plist", plain.as_bytes()),
                ("Demo.app/Contents/MacOS/Demo", b"#!/bin/sh\n"),
            ],
            "n'est pas un exécutable Mach-O",
        );
        refused(
            &[("Demo/Contents/Info.plist", plain.as_bytes())],
            "dossier Nom.app",
        );
        // L'ancien nom de la clé est encore lu, et contrôlé de même.
        let bad_keys = plist(
            "1.2.0",
            "<key>AppsMarketPublicKeys</key><string>une seule</string>",
        );
        refused(
            &[("Demo.app/Contents/Info.plist", bad_keys.as_bytes())],
            "tableau de chaînes",
        );

        fs::write(&path, b"pas une archive").unwrap();
        let error = format!("{:#}", inspect_archive(&path).unwrap_err());
        assert!(error.contains("archive illisible"), "{error}");
    }
}
