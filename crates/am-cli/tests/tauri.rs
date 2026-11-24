//! Compatibilité avec Tauri, sur des fichiers produits par le vrai `tauri signer` (tauri-cli
//! 2.12.4, voir `fixtures/tauri/README.md`) : les clés et les signatures se lisent dans les deux
//! sens.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use am_protocol::Version;
use am_protocol::signature::{PublicKey, Signature, decode_tauri};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Mot de passe public de `fixtures/tauri/tauri-generated.secret`.
const TAURI_KEY_PASSWORD: &str = "tauri-compat-essai";

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/tauri")
        .join(name)
}

fn read(name: &str) -> String {
    fs::read_to_string(data(name)).unwrap()
}

fn payload() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/files/payload.bin")
}

fn drakbase(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_drakbase"));
    command
        .env("HOME", home)
        .env_remove("APPSMARKET_HOME")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY_PASSWORD")
        .stdin(Stdio::null());
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `verify_signature` de `tauri-plugin-updater` (branche v2), recopiée : base64 strict sans
/// `trim`, minisign-verify qui accepte l'ancien format, puis le champ `version:` du commentaire
/// de confiance, comparé à la version annoncée par le serveur.
fn tauri_updater_verify(
    data: &[u8],
    signature: &str,
    pubkey: &str,
    announced: &str,
    require_signed_version: bool,
) -> Result<(), String> {
    let decode = |text: &str| -> Result<String, String> {
        let bytes = STANDARD.decode(text).map_err(|error| error.to_string())?;
        String::from_utf8(bytes).map_err(|error| error.to_string())
    };
    let public_key =
        minisign_verify::PublicKey::decode(&decode(pubkey)?).map_err(|error| error.to_string())?;
    let signature = minisign_verify::Signature::decode(&decode(signature)?)
        .map_err(|error| error.to_string())?;
    public_key
        .verify(data, &signature, true)
        .map_err(|error| error.to_string())?;
    let Some(signed) = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
    else {
        return if require_signed_version {
            Err("MissingSignedVersion".to_owned())
        } else {
            Ok(())
        };
    };
    let matches = match (
        semver::Version::parse(signed),
        semver::Version::parse(announced),
    ) {
        (Ok(signed), Ok(announced)) => signed == announced,
        _ => signed == announced,
    };
    if matches {
        Ok(())
    } else {
        Err(format!("SignedVersionMismatch : {signed} ≠ {announced}"))
    }
}

/// Tauri, avec `--app-version`, écrit exactement le commentaire de confiance de l'outil.
#[test]
fn tauri_signatures_have_our_format_and_pass_our_checks() {
    let data = fs::read(payload()).unwrap();
    let tauri_key = PublicKey::from_tauri(&read("tauri-generated.pub")).unwrap();
    let version = Version::parse_release("1.2.0").unwrap();

    let encoded = read("payload.bin.tauri.sig");
    let text = decode_tauri(&encoded).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "untrusted comment: signature from tauri secret key"
    );
    assert!(
        lines[2].starts_with("trusted comment: timestamp:"),
        "{text}"
    );
    assert!(
        lines[2].ends_with("\tfile:payload.bin\tversion:1.2.0"),
        "{text}"
    );
    let signature = Signature::from_tauri(&encoded).unwrap();
    assert!(signature.is_prehashed());
    tauri_key
        .verify(&data, &signature)
        .unwrap()
        .require_version(&version)
        .unwrap();

    // Sans --app-version, Tauri ne signe pas la version : refusé ici, comme le veut le protocole.
    let unversioned = Signature::from_tauri(&read("payload.bin.tauri-no-version.sig")).unwrap();
    let verified = tauri_key.verify(&data, &unversioned).unwrap();
    assert_eq!(verified.field("version"), None);
    assert!(verified.require_version(&version).is_err());

    // Une clé créée par `drakbase keygen` signe sous Tauri.
    let our_key = PublicKey::from_tauri(&read("appsmarket-generated.pub")).unwrap();
    let signature =
        Signature::from_tauri(&read("payload.bin.tauri-with-appsmarket-key.sig")).unwrap();
    our_key
        .verify(&data, &signature)
        .unwrap()
        .require_version(&version)
        .unwrap();
}

/// Le binaire lit les signatures de Tauri, contrôle par contrôle.
#[test]
fn verify_command_reads_tauri_signatures() {
    let home = tempfile::tempdir().unwrap();
    let verify = |signature: &str, pubkey: &str, version: Option<&str>| {
        let mut command = drakbase(home.path());
        command
            .arg("verify")
            .arg(payload())
            .arg("--signature")
            .arg(data(signature))
            .arg("--pubkey")
            .arg(data(pubkey));
        if let Some(version) = version {
            command.args(["--version", version]);
        }
        command.output().unwrap()
    };

    for (signature, pubkey) in [
        ("payload.bin.tauri.sig", "tauri-generated.pub"),
        (
            "payload.bin.tauri-with-appsmarket-key.sig",
            "appsmarket-generated.pub",
        ),
    ] {
        let output = verify(signature, pubkey, Some("1.2.0"));
        assert!(output.status.success(), "{}", stdout(&output));
        assert!(stdout(&output).contains("✓ 5."), "{}", stdout(&output));
    }

    let output = verify(
        "payload.bin.tauri-no-version.sig",
        "tauri-generated.pub",
        Some("1.2.0"),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("✗ 5."), "{}", stdout(&output));
    let output = verify(
        "payload.bin.tauri-no-version.sig",
        "tauri-generated.pub",
        None,
    );
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("! 5."), "{}", stdout(&output));
}

/// `drakbase sign` ouvre une clé créée par `tauri signer generate`, et le code de vérification
/// du plugin Tauri accepte le résultat, avec `requireSignedVersion`.
#[test]
fn our_signatures_with_a_tauri_key_pass_the_tauri_updater() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("Demo-1.2.0.app.tar.gz");
    fs::copy(payload(), &file).unwrap();
    let output = drakbase(dir.path())
        .env("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", TAURI_KEY_PASSWORD)
        .arg("sign")
        .arg(&file)
        .args(["--app", "demo", "--version", "1.2", "--key"])
        .arg(data("tauri-generated.secret"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactement les octets que le serveur renverra dans le champ `signature`.
    let signature = fs::read_to_string(dir.path().join("Demo-1.2.0.app.tar.gz.sig")).unwrap();
    let contents = fs::read(&file).unwrap();
    let pubkey = read("tauri-generated.pub");
    tauri_updater_verify(&contents, &signature, &pubkey, "1.2.0", true).unwrap();
    assert!(tauri_updater_verify(&contents, &signature, &pubkey, "1.3.0", false).is_err());
    let mut tampered = contents.clone();
    tampered[0] ^= 1;
    assert!(tauri_updater_verify(&tampered, &signature, &pubkey, "1.2.0", true).is_err());
}

/// Le même code de vérification que Tauri accepte les signatures de Tauri, et celles des jeux
/// d'essai communs, signées par `am_protocol` avec une clé de `drakbase keygen`.
#[test]
fn tauri_updater_code_accepts_both_origins() {
    let contents = fs::read(payload()).unwrap();
    tauri_updater_verify(
        &contents,
        &read("payload.bin.tauri.sig"),
        &read("tauri-generated.pub"),
        "1.2.0",
        true,
    )
    .unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    tauri_updater_verify(
        &contents,
        &fs::read_to_string(fixtures.join("files/payload.bin.sig")).unwrap(),
        &fs::read_to_string(fixtures.join("keys/test-app.pub")).unwrap(),
        "1.2.0",
        true,
    )
    .unwrap();
    assert_eq!(
        tauri_updater_verify(
            &contents,
            &read("payload.bin.tauri-no-version.sig"),
            &read("tauri-generated.pub"),
            "1.2.0",
            true,
        ),
        Err("MissingSignedVersion".to_owned())
    );
    // La clé secrète de l'outil a bien la forme de celle de Tauri.
    for name in ["tauri-generated.secret", "appsmarket-generated.secret"] {
        let text = decode_tauri(&read(name)).unwrap();
        assert!(
            text.starts_with("untrusted comment: rsign encrypted secret key\n"),
            "{name}"
        );
    }
}
