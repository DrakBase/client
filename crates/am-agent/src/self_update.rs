//! `self-update` : drakbase-stack est l'app `drakbase-stack`, de type `cli`, avec un artefact
//! `bin` (docs/PROTOCOLE.md, « Exécutable `bin` »). La signature est vérifiée avec les clés
//! publiques embarquées à la compilation (`drakbase.json`), puis l'exécutable est remplacé d'un
//! coup : écriture à côté, puis renommage.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use am_protocol::{
    Arch, BundleType, Channel, ServerUrl, Version, elf_arch, elf_is_executable, human_size, is_elf,
};
use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::files;
use crate::http::{Check, Http};
use crate::outcome::Outcome;
use crate::trust;
use crate::update::{Agent, machine_arch};

include!(concat!(env!("OUT_DIR"), "/embedded_keys.rs"));

/// Slug de l'agent sur le serveur.
pub const APP: &str = "drakbase-stack";
/// Un exécutable statique de l'agent pèse quelques Mio.
const MAX_BIN_DOWNLOAD: u64 = 256 * 1024 * 1024;

pub fn run(agent: &Agent) -> Result<Outcome> {
    let keys = usable_keys(EMBEDDED_PUBLIC_KEYS)?;
    // Le minuteur et une commande à la main ne remplacent pas l'exécutable en même temps.
    let _lock = files::lock_agent(&agent.paths.state_dir.join(".self-update.lock"), APP)?;
    let config = Config::load_or_create(&agent.paths, None)?;
    let exe = std::env::current_exe().context("chemin de drakbase-stack")?;
    let current = Version::parse_release(env!("CARGO_PKG_VERSION"))?;
    match apply(
        &Http::new()?,
        &config.server,
        &keys,
        &exe,
        &current,
        machine_arch()?,
        config.install_id(),
    )? {
        Some(version) => println!("drakbase-stack mis à jour : {current} → {version}"),
        None => println!("drakbase-stack est à jour ({current})"),
    }
    Ok(Outcome::Done)
}

/// Les clés qui vérifieront la nouvelle version : celles que l'exécutable embarque. Sans aucune
/// clé, rien ne pourrait être vérifié : c'est un refus, dit avant toute requête au serveur.
fn usable_keys(embedded: &[&str]) -> Result<Vec<String>> {
    if embedded.is_empty() {
        bail!(
            "cet exécutable a été compilé sans clé publique (crates/am-agent/drakbase.json, écrit par drakbase init) : il ne peut pas vérifier une nouvelle version, et refuse de se mettre à jour. Installez la nouvelle version à la main (/install/drakbase-stack.sh)."
        );
    }
    Ok(embedded.iter().map(|key| (*key).to_owned()).collect())
}

/// Cherche, vérifie et met en place une version plus récente de l'exécutable `exe`. Rend la
/// version installée, ou `None` si tout est à jour.
pub fn apply(
    http: &Http,
    server: &ServerUrl,
    keys: &[String],
    exe: &Path,
    current: &Version,
    arch: Arch,
    install_id: Option<&str>,
) -> Result<Option<Version>> {
    let Some(offer) = http.check(&Check {
        server,
        app: APP,
        arch,
        version: current,
        bundle_type: BundleType::Bin,
        channel: Channel::Stable,
        os_version: None,
        install_id,
    })?
    else {
        return Ok(None);
    };
    if offer.version <= *current || !Channel::Stable.includes(offer.channel) {
        return Ok(None);
    }
    let version = offer.version.clone();
    if offer.size > MAX_BIN_DOWNLOAD {
        bail!(
            "la version {version} annonce un exécutable de {}",
            human_size(offer.size)
        );
    }
    let partial = files::sibling(exe, ".partial");
    http.download(server, &offer.url, &partial, offer.size, &offer.sha256)
        .with_context(|| format!("version {version} de drakbase-stack refusée"))?;
    let installed = (|| -> Result<()> {
        let bytes =
            fs::read(&partial).with_context(|| format!("lecture de {}", partial.display()))?;
        trust::verify(keys, &bytes, &offer.signature, &version)?;
        check_elf(&bytes, arch)?;
        fs::set_permissions(&partial, Permissions::from_mode(0o755))?;
        fs::rename(&partial, exe).with_context(|| format!("remplacement de {}", exe.display()))
    })();
    if let Err(error) = installed {
        let _ = fs::remove_file(&partial);
        return Err(error.context(format!("version {version} de drakbase-stack refusée")));
    }
    Ok(Some(version))
}

/// Contrôle que `bytes` est un exécutable ELF pour `arch` : un fichier objet (`.o`) est un ELF,
/// mais ne s'exécute pas.
pub fn check_elf(bytes: &[u8], arch: Arch) -> Result<()> {
    if !is_elf(bytes) {
        bail!("ce n'est pas un exécutable ELF");
    }
    if !elf_is_executable(bytes) {
        bail!("fichier ELF qui n'est pas un exécutable");
    }
    if elf_arch(bytes) != Some(arch) {
        bail!("exécutable ELF d'une autre architecture que {arch}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use am_protocol::signature::PublicKey;
    use am_protocol::signing;

    use super::*;
    use crate::fake_server::{FakeRelease, FakeServer};

    fn fixture(name: &str) -> String {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/keys")
            .join(name);
        fs::read_to_string(path).unwrap().trim().to_owned()
    }

    /// `e_type` d'un exécutable à adresse fixe, d'un exécutable indépendant de sa position (PIE)
    /// et d'un fichier objet (`.o`).
    const ET_EXEC: u16 = 2;
    const ET_DYN: u16 = 3;
    const ET_REL: u16 = 1;

    /// En-tête ELF minimal d'un exécutable pour `machine`, suivi d'un contenu reconnaissable.
    fn elf(machine: u16, payload: &[u8]) -> Vec<u8> {
        elf_of_type(ET_EXEC, machine, payload)
    }

    fn elf_of_type(kind: u16, machine: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0];
        bytes.resize(16, 0);
        bytes.extend(kind.to_le_bytes());
        bytes.extend(machine.to_le_bytes());
        bytes.resize(64, 0);
        bytes.extend(payload);
        bytes
    }

    fn signed(bytes: &[u8], version: &str, secret: &str) -> String {
        signing::sign(
            &fixture(secret),
            "appsmarket-fixtures",
            bytes,
            "drakbase-stack-bin",
            &Version::parse_release(version).unwrap(),
            1_767_225_600,
        )
        .unwrap()
    }

    #[test]
    fn embedded_keys_are_valid() {
        for key in EMBEDDED_PUBLIC_KEYS {
            PublicKey::from_tauri(key).expect("clé de drakbase.json illisible");
        }
    }

    #[test]
    fn refuses_without_an_embedded_key() {
        let error = usable_keys(&[]).unwrap_err().to_string();
        assert!(error.contains("compilé sans clé publique"), "{error}");
        assert_eq!(usable_keys(&["une clé"]).unwrap(), ["une clé"]);
    }

    #[test]
    fn elf_checks() {
        assert!(check_elf(&elf(183, b""), Arch::Aarch64).is_ok());
        assert!(check_elf(&elf_of_type(ET_DYN, 183, b""), Arch::Aarch64).is_ok());
        assert!(check_elf(&elf(62, b""), Arch::Aarch64).is_err());
        assert!(check_elf(b"#!/bin/sh\necho\n", Arch::X86_64).is_err());
        // Un fichier objet de la bonne arch est un ELF, mais pas un exécutable.
        let error = check_elf(&elf_of_type(ET_REL, 183, b""), Arch::Aarch64).unwrap_err();
        assert_eq!(error.to_string(), "fichier ELF qui n'est pas un exécutable");
    }

    #[test]
    fn replaces_the_executable_after_checks() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("drakbase-stack");
        fs::write(&exe, b"ancien").unwrap();
        let keys = vec![fixture("test-app.pub")];
        let current = Version::parse_release("0.3.0").unwrap();
        let http = Http::new().unwrap();

        let good = elf(183, b"nouveau");
        let server = FakeServer::start();
        server.declare(APP);
        let url = ServerUrl::parse(&server.url).unwrap();

        // Rien de publié : à jour.
        assert_eq!(
            apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).unwrap(),
            None
        );

        // Signée par une autre clé : refusée, l'exécutable ne change pas.
        server.publish(FakeRelease::bin(
            APP,
            "0.4.0",
            Arch::Aarch64,
            good.clone(),
            signed(&good, "0.4.0", "other-app.secret"),
        ));
        let error = apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("clé de confiance"),
            "{error:#}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"ancien");
        assert!(!files::sibling(&exe, ".partial").exists());

        // Exécutable d'une autre arch, bien signé : refusé.
        server.clear();
        let other_arch = elf(62, b"x86");
        server.publish(FakeRelease::bin(
            APP,
            "0.4.0",
            Arch::Aarch64,
            other_arch.clone(),
            signed(&other_arch, "0.4.0", "test-app.secret"),
        ));
        assert!(apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"ancien");

        // Fichier objet de la bonne arch, bien signé : refusé.
        server.clear();
        let object = elf_of_type(ET_REL, 183, b"objet");
        server.publish(FakeRelease::bin(
            APP,
            "0.4.0",
            Arch::Aarch64,
            object.clone(),
            signed(&object, "0.4.0", "test-app.secret"),
        ));
        let error = apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("fichier ELF qui n'est pas un exécutable"),
            "{error:#}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"ancien");
        assert!(!files::sibling(&exe, ".partial").exists());

        // La bonne : mise en place, exécutable. Un lien posé à la place du fichier de travail
        // n'est pas suivi : sa cible reste telle quelle.
        let victim = dir.path().join("victime");
        fs::write(&victim, b"intact").unwrap();
        std::os::unix::fs::symlink(&victim, files::sibling(&exe, ".partial")).unwrap();
        server.clear();
        server.publish(FakeRelease::bin(
            APP,
            "0.4.0",
            Arch::Aarch64,
            good.clone(),
            signed(&good, "0.4.0", "test-app.secret"),
        ));
        let installed = apply(
            &http,
            &url,
            &keys,
            &exe,
            &current,
            Arch::Aarch64,
            Some("0b6d3c2e-4f1a-4b8c-9d2e-1f3a5b7c9d0e"),
        )
        .unwrap();
        assert_eq!(installed, Some(Version::parse_release("0.4.0").unwrap()));
        assert_eq!(fs::read(&exe).unwrap(), good);
        assert_eq!(fs::read(&victim).unwrap(), b"intact");
        assert!(!fs::symlink_metadata(&exe).unwrap().is_symlink());
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let checks = server.checks();
        let last = checks.last().unwrap();
        assert_eq!(
            last.path,
            "/api/v1/update/drakbase-stack/linux/aarch64/0.3.0"
        );
        assert_eq!(
            last.query.get("bundle_type").map(String::as_str),
            Some("bin")
        );
        assert_eq!(
            last.install_id.as_deref(),
            Some("0b6d3c2e-4f1a-4b8c-9d2e-1f3a5b7c9d0e")
        );

        // Exécutable annoncé au-delà de la limite : refusé sans être téléchargé, sa taille dite
        // en unités binaires.
        server.clear();
        let mut huge = FakeRelease::bin(
            APP,
            "0.5.0",
            Arch::Aarch64,
            good.clone(),
            signed(&good, "0.5.0", "test-app.secret"),
        );
        huge.announced_size = Some(300 * 1024 * 1024);
        server.publish(huge);
        let downloads = server.downloads().len();
        let error = apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "la version 0.5.0 annonce un exécutable de 300,0 Mio"
        );
        assert_eq!(server.downloads().len(), downloads);

        // Version signée différente de celle annoncée : refusée.
        server.clear();
        server.publish(FakeRelease::bin(
            APP,
            "0.5.0",
            Arch::Aarch64,
            good.clone(),
            signed(&good, "0.4.0", "test-app.secret"),
        ));
        let error = apply(&http, &url, &keys, &exe, &current, Arch::Aarch64, None).unwrap_err();
        assert!(format!("{error:#}").contains("version signée"), "{error:#}");
    }
}
