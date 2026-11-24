//! Artefacts à envoyer : leur emplacement (OS, arch, type), donné sous la forme
//! `darwin/universal/app=Nom-1.2.0.app.tar.gz` ou déduit du fichier, et leur signature `.sig`, lue
//! à côté (sauf pour un dmg). Une archive macOS est lue pour son identifiant, sa version et ses
//! processeurs, qui donnent aussi son arch. Une archive de stack (`.stack.tar.gz`) est lue et
//! contrôlée comme le fera le serveur, et un exécutable Linux (`bin`) se reconnaît à son en-tête
//! ELF, qui donne son arch et dit s'il s'exécute : un fichier objet est refusé ici, comme le
//! serveur le refuserait une fois reçu.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use am_protocol::elf::ELF_HEADER_LEN;
use am_protocol::names::MAX_FILENAME_CHARS;
use am_protocol::signature::Signature;
use am_protocol::stack::manifest::StackManifest;
use am_protocol::stack::{self, docker_platform};
use am_protocol::{Arch, BundleType, Os, Version};
use anyhow::{Context, Result, bail, ensure};

use crate::files;
use crate::home::AppSlug;
use crate::macos::{self, ArchiveContents};

/// Emplacement d'un artefact dans une version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub os: Os,
    pub arch: Arch,
    pub bundle_type: BundleType,
}

impl Slot {
    /// Refuse un type d'un autre OS (`windows/x86_64/dmg`) ou une arch absente de cet OS.
    pub fn new(os: Os, arch: Arch, bundle_type: BundleType) -> Result<Self> {
        ensure!(
            bundle_type.os() == os,
            "emplacement {os}/{arch}/{bundle_type} : le type {bundle_type} est un artefact {}",
            bundle_type.os()
        );
        ensure!(
            arch.supported_on(os),
            "emplacement {os}/{arch}/{bundle_type} : pas d'arch {arch} pour {os}"
        );
        Ok(Self {
            os,
            arch,
            bundle_type,
        })
    }

    fn parse(text: &str) -> Result<Self> {
        let parts: Vec<&str> = text.split('/').collect();
        let [os, arch, bundle_type] = parts.as_slice() else {
            bail!(
                "emplacement « {text} » : <os>/<arch>/<type> attendu, par exemple darwin/universal/app"
            );
        };
        let unknown =
            |error: am_protocol::UnknownValue| anyhow::anyhow!("emplacement « {text} » : {error}");
        Self::new(
            os.parse().map_err(unknown)?,
            arch.parse().map_err(unknown)?,
            bundle_type.parse().map_err(unknown)?,
        )
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.os, self.arch, self.bundle_type)
    }
}

/// Un artefact prêt à partir.
#[derive(Debug)]
pub struct LocalArtifact {
    pub slot: Slot,
    pub path: PathBuf,
    /// Nom du fichier, tel qu'il est sur le disque.
    pub file_name: String,
    /// Nom envoyé dans `X-AppsMarket-Filename`, réduit à `[A-Za-z0-9._-]` comme le fait le serveur.
    pub upload_name: String,
    pub size: u64,
    /// Contenu du `.sig`, sans blanc autour ; absent pour un dmg.
    pub signature: Option<String>,
    /// Pour une archive macOS : ce qu'elle contient.
    pub contents: Option<ArchiveContents>,
    /// Pour une archive de stack : son `stack.json`, publié.
    pub stack: Option<StackManifest>,
}

/// Un artefact désigné : son fichier, et son emplacement s'il est donné.
#[derive(Clone, Debug)]
pub struct Designation {
    slot: Option<Slot>,
    path: PathBuf,
}

impl Designation {
    /// `os/arch/type=chemin`, ou le chemin seul.
    pub fn parse(text: &str) -> Result<Self> {
        if let Some((prefix, path)) = text.split_once('=') {
            let looks_like_slot = prefix.split('/').count() == 3
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_'));
            if looks_like_slot {
                ensure!(!path.is_empty(), "« {text} » : fichier manquant après =");
                return Ok(Self {
                    slot: Some(Slot::parse(prefix)?),
                    path: PathBuf::from(path),
                });
            }
        }
        Ok(Self {
            slot: None,
            path: PathBuf::from(text),
        })
    }

    pub fn at(slot: Slot, path: PathBuf) -> Self {
        Self {
            slot: Some(slot),
            path,
        }
    }
}

/// Lit les artefacts désignés et contrôle tout ce qui peut l'être sans le serveur : emplacements
/// distincts, archive macOS ou de stack lisible et à la bonne version, signature présente.
pub fn prepare(
    designations: &[Designation],
    app: &AppSlug,
    version: &Version,
) -> Result<Vec<LocalArtifact>> {
    let mut found = Vec::new();
    for designation in designations {
        found.push(read(designation, app, version)?);
    }
    resolve_dmg_archs(&mut found)?;

    let mut artifacts: Vec<LocalArtifact> = Vec::new();
    for item in found {
        let arch = item.arch.expect("arch résolue");
        let slot = Slot::new(item.bundle_type.os(), arch, item.bundle_type)
            .with_context(|| item.path.display().to_string())?;
        if let Some(other) = artifacts.iter().find(|artifact| artifact.slot == slot) {
            bail!(
                "{} et {} vont tous deux à l'emplacement {slot} : une version n'a qu'un artefact par emplacement",
                other.path.display(),
                item.path.display()
            );
        }
        let signature = if slot.bundle_type == BundleType::Dmg {
            None
        } else {
            Some(read_signature(&item.path, version)?)
        };
        artifacts.push(LocalArtifact {
            slot,
            upload_name: upload_name(&item.file_name)
                .with_context(|| item.path.display().to_string())?,
            path: item.path,
            file_name: item.file_name,
            size: item.size,
            signature,
            contents: item.contents,
            stack: item.stack,
        });
    }
    Ok(artifacts)
}

/// Un artefact lu, dont l'arch peut rester à déduire (dmg).
struct Found {
    path: PathBuf,
    file_name: String,
    size: u64,
    bundle_type: BundleType,
    arch: Option<Arch>,
    contents: Option<ArchiveContents>,
    stack: Option<StackManifest>,
}

fn read(designation: &Designation, app: &AppSlug, version: &Version) -> Result<Found> {
    let Designation { slot, path } = designation.clone();
    let metadata = fs::metadata(&path).with_context(|| format!("artefact {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} : nom de fichier illisible", path.display()))?
        .to_owned();
    if metadata.is_dir() {
        if file_name.ends_with(".app") {
            bail!(
                "{} est un bundle : empaquette-le d'abord avec « drakbase pack-macos », ou publie-le d'un coup avec « drakbase release »",
                path.display()
            );
        }
        bail!("{} est un dossier, pas un fichier", path.display());
    }
    ensure!(metadata.len() > 0, "{} est vide", path.display());
    if file_name.to_ascii_lowercase().ends_with(".sig") {
        bail!(
            "{} est une signature : donne le fichier signé, sa .sig est lue à côté",
            path.display()
        );
    }

    let elf = elf_header(&path)?;
    let bundle_type = match slot {
        Some(slot) => slot.bundle_type,
        None => match (bundle_type_of(&file_name), &elf) {
            (Some(bundle_type), _) => bundle_type,
            // Un exécutable Linux sans extension reconnue : celui d'une app cli.
            (None, Some(_)) => BundleType::Bin,
            (None, None) => bail!(
                "type d'artefact inconnu pour « {file_name} » : .app.tar.gz, .dmg, .exe (NSIS), .msi, .AppImage, .deb, .rpm, .stack.tar.gz ou un exécutable Linux attendu, ou un emplacement donné, par exemple linux/x86_64/appimage={file_name}"
            ),
        },
    };
    let contents = match bundle_type {
        BundleType::App => {
            Some(macos::inspect_archive(&path).with_context(|| format!("{}", path.display()))?)
        }
        _ => None,
    };
    let stack = match bundle_type {
        BundleType::Stack => Some(read_stack(&path, metadata.len(), app, version)?),
        _ => None,
    };
    let elf = match (bundle_type, elf) {
        (BundleType::Bin, None) => bail!(
            "{} n'est pas un exécutable Linux (ELF) : un artefact bin l'est",
            path.display()
        ),
        (BundleType::Bin, Some(elf)) => {
            let arch = elf.arch.with_context(|| {
                format!(
                    "{} : architecture de l'exécutable non prise en charge",
                    path.display()
                )
            })?;
            ensure!(
                elf.executable,
                "{} est un fichier ELF qui n'est pas un exécutable (un fichier objet .o, par exemple) : le serveur le refuserait. Donne l'exécutable lié, statique ou non.",
                path.display()
            );
            Some(arch)
        }
        _ => None,
    };
    if let Some(contents) = &contents {
        let info = &contents.info;
        ensure!(
            info.version == *version,
            "{} contient la version {}{}, pas {version}",
            path.display(),
            info.version,
            if info.raw_version == info.version.to_string() {
                String::new()
            } else {
                format!(" (CFBundleShortVersionString « {} »)", info.raw_version)
            }
        );
    }

    let named = arch_in_name(&file_name, bundle_type).map_err(|archs| {
        anyhow::anyhow!(
            "« {file_name} » nomme plusieurs architectures ({}) : donne son emplacement, par exemple {}={file_name}",
            archs.iter().map(|arch| arch.as_str()).collect::<Vec<_>>().join(", "),
            example_slot(bundle_type)
        )
    })?;
    let detected = contents.as_ref().map(|contents| &contents.cpus);
    let arch = match (slot.map(|slot| slot.arch), named, detected) {
        (Some(arch), _, _) | (None, Some(arch), _) if elf.is_some_and(|elf| elf != arch) => bail!(
            "{} : l'exécutable est fait pour {}, pas pour l'arch {arch}",
            path.display(),
            elf.unwrap_or(arch)
        ),
        (None, None, None) if elf.is_some() => elf,
        (Some(arch), _, _) | (None, Some(arch), _) => {
            if let Some(cpus) = detected {
                ensure!(
                    cpus.serve(arch),
                    "{} : l'exécutable contient {cpus}, il ne convient pas à l'arch {arch}{}",
                    path.display(),
                    cpus.arch()
                        .map(|right| format!(" (son arch : {right})"))
                        .unwrap_or_default()
                );
            }
            Some(arch)
        }
        (None, None, Some(cpus)) => Some(cpus.arch().with_context(|| {
            format!(
                "{} : l'exécutable contient {cpus}, ni arm64 ni x86_64",
                path.display()
            )
        })?),
        (None, None, None) if bundle_type == BundleType::Dmg => None,
        (None, None, None) => bail!(
            "impossible de déduire l'architecture de « {file_name} » : donne son emplacement, par exemple {}={file_name}",
            example_slot(bundle_type)
        ),
    };
    if let (Some(manifest), Some(arch)) = (&stack, arch) {
        ensure!(
            manifest.platforms.contains(&arch),
            "{} : la stack ne se construit pas pour {arch} ({}), absente des platforms de son stack.json",
            path.display(),
            docker_platform(arch).unwrap_or("?")
        );
    }
    Ok(Found {
        path,
        file_name,
        size: metadata.len(),
        bundle_type,
        arch,
        contents,
        stack,
    })
}

/// Lit une archive de stack et son `stack.json` comme le serveur : règles de l'archive, puis
/// lecture « publiée », qui doit être celle de cette app et de cette version.
fn read_stack(path: &Path, size: u64, app: &AppSlug, version: &Version) -> Result<StackManifest> {
    ensure!(
        size <= stack::archive::MAX_ARCHIVE_BYTES,
        "{} : trop gros pour une archive de stack ({})",
        path.display(),
        am_protocol::human_size(size)
    );
    let bytes = fs::read(path).with_context(|| format!("lecture de {}", path.display()))?;
    let archive = stack::archive::read(&bytes)
        .map_err(|error| anyhow::anyhow!("{} : {error} ({})", path.display(), error.code()))?;
    // Le compose de l'archive, contrôlé tel qu'il est écrit, comme le fera l'agent : l'envoi d'une
    // archive qu'il refuserait ne sert à rien.
    stack::compose::check_archived(archive.compose_yaml())
        .map_err(|error| anyhow::anyhow!("{} : {error} ({})", path.display(), error.code()))?;
    if let Some(env_example) = archive.env_example() {
        stack::compose::check_env_example(env_example)
            .map_err(|error| anyhow::anyhow!("{} : {error} ({})", path.display(), error.code()))?;
    }
    StackManifest::parse_published(archive.stack_json(), &app.to_string(), version)
        .map_err(|error| anyhow::anyhow!("{} : {error} ({})", path.display(), error.code()))
}

/// Ce que l'en-tête ELF d'un fichier dit de lui.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ElfHeader {
    /// Son arch (`am_protocol::elf_arch`) ; `None` hors de la table.
    arch: Option<Arch>,
    /// Il s'exécute (`am_protocol::elf_is_executable`) : ni fichier objet, ni image mémoire.
    executable: bool,
}

/// L'en-tête ELF d'un fichier ; `None` s'il n'est pas un ELF.
fn elf_header(path: &Path) -> Result<Option<ElfHeader>> {
    let mut head = Vec::with_capacity(ELF_HEADER_LEN);
    File::open(path)
        .and_then(|file| file.take(ELF_HEADER_LEN as u64).read_to_end(&mut head))
        .with_context(|| format!("lecture de {}", path.display()))?;
    // Un en-tête tronqué n'est pas celui d'un exécutable.
    let elf = head.len() == ELF_HEADER_LEN && am_protocol::is_elf(&head);
    Ok(elf.then(|| ElfHeader {
        arch: am_protocol::elf_arch(&head),
        executable: am_protocol::elf_is_executable(&head),
    }))
}

/// Fins de nom reconnues, comme les écrivent `pack-macos`, `tauri build` et `drakbase release`.
const SUFFIXES: [(&str, BundleType); 8] = [
    (".stack.tar.gz", BundleType::Stack),
    (".app.tar.gz", BundleType::App),
    (".dmg", BundleType::Dmg),
    (".exe", BundleType::Nsis),
    (".msi", BundleType::Msi),
    (".appimage", BundleType::AppImage),
    (".deb", BundleType::Deb),
    (".rpm", BundleType::Rpm),
];

fn bundle_type_of(file_name: &str) -> Option<BundleType> {
    let lower = file_name.to_ascii_lowercase();
    SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.len() > suffix.len() && lower.ends_with(suffix))
        .map(|(_, bundle_type)| *bundle_type)
}

/// Le nom sans sa fin reconnue : `Translate-1.2.0` pour `Translate-1.2.0.app.tar.gz`.
fn stem(file_name: &str) -> &str {
    let lower = file_name.to_ascii_lowercase();
    SUFFIXES
        .iter()
        .find(|(suffix, _)| lower.ends_with(suffix))
        .map_or(file_name, |(suffix, _)| {
            &file_name[..file_name.len() - suffix.len()]
        })
}

/// Arch écrite dans le nom, comme le font Tauri et les paquets Linux (`_aarch64`, `_x64-setup`,
/// `_amd64.deb`, `.x86_64.rpm`…). `Err` avec les archs trouvées si le nom en donne plusieurs.
fn arch_in_name(file_name: &str, bundle_type: BundleType) -> Result<Option<Arch>, Vec<Arch>> {
    // x86_64 et x86-64 seraient coupés en « x86 » (i686 chez Tauri) et « 64 ».
    let lower = stem(file_name)
        .to_ascii_lowercase()
        .replace("x86_64", "x64")
        .replace("x86-64", "x64");
    let mut found: Vec<Arch> = Vec::new();
    for token in lower.split(['_', '-', '.', ' ']) {
        let arch = match token {
            "aarch64" | "arm64" => Arch::Aarch64,
            "x64" | "amd64" => Arch::X86_64,
            "universal" => Arch::Universal,
            "x86" | "i686" | "i386" => Arch::I686,
            "armv7" | "armv7l" | "armhf" | "armhfp" => Arch::Armv7,
            "riscv64" => Arch::Riscv64,
            _ => continue,
        };
        if arch.supported_on(bundle_type.os()) && !found.contains(&arch) {
            found.push(arch);
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [arch] => Ok(Some(*arch)),
        _ => Err(found),
    }
}

fn example_slot(bundle_type: BundleType) -> String {
    let arch = match bundle_type.os() {
        Os::Darwin => Arch::Universal,
        Os::Windows | Os::Linux => Arch::X86_64,
    };
    format!("{}/{arch}/{bundle_type}", bundle_type.os())
}

/// Un dmg sans arch dans son nom prend celle de l'archive `.app.tar.gz` de même nom, donnée dans
/// la même commande : c'est ce que produit `pack-macos --dmg`.
fn resolve_dmg_archs(found: &mut [Found]) -> Result<()> {
    for index in 0..found.len() {
        if found[index].arch.is_some() {
            continue;
        }
        let name = stem(&found[index].file_name).to_owned();
        let archs: Vec<Arch> = found
            .iter()
            .filter(|other| other.bundle_type == BundleType::App && stem(&other.file_name) == name)
            .filter_map(|other| other.arch)
            .collect();
        match archs.as_slice() {
            [arch] => found[index].arch = Some(*arch),
            _ => bail!(
                "impossible de déduire l'architecture de « {} » : donne son emplacement, par exemple darwin/universal/dmg={}, ou donne aussi l'archive {name}.app.tar.gz",
                found[index].file_name,
                found[index].path.display()
            ),
        }
    }
    Ok(())
}

/// Lit `<fichier>.sig` et contrôle sa forme ; la signature elle-même est vérifiée avec les clés
/// de l'app, connues du serveur.
fn read_signature(path: &Path, version: &Version) -> Result<String> {
    let sig_path = files::with_suffix(path, ".sig");
    let text = match fs::read_to_string(&sig_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "signature {} introuvable : signe d'abord le fichier avec « drakbase sign {} --app <app> --version {version} »",
            sig_path.display(),
            path.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("lecture de {}", sig_path.display()));
        }
    };
    let text = text.trim().to_owned();
    Signature::from_tauri(&text).with_context(|| {
        format!(
            "{} : signature illisible (format Tauri attendu)",
            sig_path.display()
        )
    })?;
    Ok(text)
}

/// Le nom réduit comme le fait le serveur (`am_protocol::sanitize_filename`). Le serveur
/// couperait un nom trop long ; l'outil le refuse, puisque ce nom entre dans la signature.
fn upload_name(file_name: &str) -> Result<String> {
    ensure!(
        file_name.chars().count() <= MAX_FILENAME_CHARS,
        "nom de fichier trop long pour le serveur ({MAX_FILENAME_CHARS} caractères au plus) : renomme-le"
    );
    Ok(am_protocol::sanitize_filename(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_parsed_and_checked() {
        let given = Designation::parse("darwin/universal/app=dist/A-1.2.0.app.tar.gz").unwrap();
        assert_eq!(given.slot.unwrap().to_string(), "darwin/universal/app");
        assert_eq!(given.path, PathBuf::from("dist/A-1.2.0.app.tar.gz"));
        // Un « = » dans un nom de fichier ordinaire n'est pas un emplacement.
        let plain = Designation::parse("dist/a=b.dmg").unwrap();
        assert!(plain.slot.is_none());
        assert_eq!(plain.path, PathBuf::from("dist/a=b.dmg"));
        for (designation, message) in [
            ("windows/x86_64/dmg=a.dmg", "artefact darwin"),
            (
                "linux/universal/appimage=a.AppImage",
                "pas d'arch universal",
            ),
            ("darwin/arm64/app=a.app.tar.gz", "architecture"),
            ("darwin/universal/app=", "fichier manquant"),
        ] {
            let error = Designation::parse(designation).unwrap_err().to_string();
            assert!(error.contains(message), "{designation} : {error}");
        }
    }

    #[test]
    fn types_and_archs_come_from_tauri_and_pack_names() {
        for (name, bundle_type, arch) in [
            ("Translate-1.2.0.app.tar.gz", BundleType::App, None),
            ("Translate-1.2.0.dmg", BundleType::Dmg, None),
            (
                "Demo_0.1.0_aarch64.dmg",
                BundleType::Dmg,
                Some(Arch::Aarch64),
            ),
            (
                "Demo_0.1.0_universal.dmg",
                BundleType::Dmg,
                Some(Arch::Universal),
            ),
            (
                "Demo_0.1.0_x64-setup.exe",
                BundleType::Nsis,
                Some(Arch::X86_64),
            ),
            (
                "Demo_0.1.0_x86-setup.exe",
                BundleType::Nsis,
                Some(Arch::I686),
            ),
            (
                "Demo_0.1.0_arm64-setup.exe",
                BundleType::Nsis,
                Some(Arch::Aarch64),
            ),
            (
                "Demo_0.1.0_x64_fr-FR.msi",
                BundleType::Msi,
                Some(Arch::X86_64),
            ),
            (
                "demo_0.1.0_amd64.AppImage",
                BundleType::AppImage,
                Some(Arch::X86_64),
            ),
            (
                "demo_0.1.0_aarch64.AppImage",
                BundleType::AppImage,
                Some(Arch::Aarch64),
            ),
            ("demo_0.1.0_armhf.deb", BundleType::Deb, Some(Arch::Armv7)),
            (
                "demo-0.1.0-1.x86_64.rpm",
                BundleType::Rpm,
                Some(Arch::X86_64),
            ),
            (
                "demo-0.1.0-1.riscv64.rpm",
                BundleType::Rpm,
                Some(Arch::Riscv64),
            ),
            // Les archives de stack qu'écrit drakbase release.
            (
                "home-manager-0.2.0-aarch64.stack.tar.gz",
                BundleType::Stack,
                Some(Arch::Aarch64),
            ),
            (
                "home-manager-0.2.0-x86_64.stack.tar.gz",
                BundleType::Stack,
                Some(Arch::X86_64),
            ),
        ] {
            assert_eq!(bundle_type_of(name), Some(bundle_type), "{name}");
            assert_eq!(arch_in_name(name, bundle_type), Ok(arch), "{name}");
        }
        assert_eq!(
            stem("home-manager-0.2.0-armv7.stack.tar.gz"),
            "home-manager-0.2.0-armv7"
        );
        assert_eq!(bundle_type_of("Demo.zip"), None);
        assert_eq!(bundle_type_of(".dmg"), None);
        assert_eq!(bundle_type_of("Demo.nsis.zip"), None);
        // « universal » n'existe que pour macOS ; deux archs différentes ne se devinent pas.
        assert_eq!(
            arch_in_name("universal_0.1.0_amd64.deb", BundleType::Deb),
            Ok(Some(Arch::X86_64))
        );
        assert_eq!(
            arch_in_name("Demo_aarch64_x64.dmg", BundleType::Dmg),
            Err(vec![Arch::Aarch64, Arch::X86_64])
        );
        assert_eq!(stem("Translate-1.2.0.app.tar.gz"), "Translate-1.2.0");
        assert_eq!(stem("Translate-1.2.0.DMG"), "Translate-1.2.0");
    }

    #[test]
    fn executables_are_recognized_by_their_elf_header() {
        let dir = tempfile::tempdir().unwrap();
        // Un en-tête ELF de cette classe, de ce boutisme, de ce type et de cette machine.
        let elf = |class: u8, data: u8, kind: u16, machine: u16| {
            let mut bytes = vec![0x7f, b'E', b'L', b'F', class, data, 1];
            bytes.resize(16, 0);
            for word in [kind, machine] {
                match data {
                    2 => bytes.extend(word.to_be_bytes()),
                    _ => bytes.extend(word.to_le_bytes()),
                }
            }
            bytes.resize(64, 0);
            bytes
        };
        let header = |arch: Option<Arch>, executable: bool| Some(ElfHeader { arch, executable });
        for (bytes, expected) in [
            (elf(2, 1, 2, 0xb7), header(Some(Arch::Aarch64), true)),
            (elf(2, 1, 2, 0x3e), header(Some(Arch::X86_64), true)),
            (elf(1, 1, 2, 0x28), header(Some(Arch::Armv7), true)),
            (elf(2, 1, 2, 0xf3), header(Some(Arch::Riscv64), true)),
            (elf(1, 1, 2, 0x03), header(Some(Arch::I686), true)),
            // Indépendant de sa position (PIE) : un exécutable lui aussi.
            (elf(2, 1, 3, 0xb7), header(Some(Arch::Aarch64), true)),
            // Un fichier objet, une image mémoire, un type absent : des ELF qui ne s'exécutent pas.
            (elf(2, 1, 1, 0xb7), header(Some(Arch::Aarch64), false)),
            (elf(2, 1, 4, 0x3e), header(Some(Arch::X86_64), false)),
            (elf(2, 1, 0, 0x3e), header(Some(Arch::X86_64), false)),
            // PowerPC 64 bits, gros-boutien : un ELF, d'une arch hors de la table.
            (elf(2, 2, 2, 0x15), header(None, true)),
            (b"#!/bin/sh\necho bonjour\n".to_vec(), None),
            (b"\x7fELF".to_vec(), None),
        ] {
            let path = dir.path().join("outil");
            fs::write(&path, &bytes).unwrap();
            assert_eq!(elf_header(&path).unwrap(), expected, "{bytes:?}");
        }
    }

    #[test]
    fn an_elf_that_is_not_an_executable_is_refused_as_a_bin() {
        let dir = tempfile::tempdir().unwrap();
        let app: AppSlug = "demo".parse().unwrap();
        let version = Version::parse_release("0.4.0").unwrap();
        let write = |name: &str, kind: u8| {
            let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
            bytes.resize(64, 0);
            bytes[16] = kind;
            bytes[18] = 0xb7;
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        // Un exécutable sans extension : l'emplacement se déduit de son en-tête.
        let found = read(
            &Designation::parse(write("agent", 2).to_str().unwrap()).unwrap(),
            &app,
            &version,
        )
        .unwrap();
        assert_eq!(
            (found.bundle_type, found.arch),
            (BundleType::Bin, Some(Arch::Aarch64))
        );
        // Un fichier objet, avec ou sans emplacement donné.
        let object = write("agent.o", 1);
        for designation in [
            object.display().to_string(),
            format!("linux/aarch64/bin={}", object.display()),
        ] {
            let error = read(&Designation::parse(&designation).unwrap(), &app, &version)
                .err()
                .unwrap()
                .to_string();
            assert!(
                error.contains("agent.o est un fichier ELF qui n'est pas un exécutable"),
                "{error}"
            );
        }
    }

    #[test]
    fn upload_names_are_reduced_like_the_server_does() {
        assert_eq!(
            upload_name("Translate-1.2.0.app.tar.gz").unwrap(),
            "Translate-1.2.0.app.tar.gz"
        );
        assert_eq!(
            upload_name("Mon Appli é-1.0.0.dmg").unwrap(),
            "Mon_Appli__-1.0.0.dmg"
        );
        assert!(upload_name(&"a".repeat(201)).is_err());
    }
}
