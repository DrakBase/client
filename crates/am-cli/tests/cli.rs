//! Essais du binaire `drakbase`, lancé comme à la main dans des dossiers temporaires. `HOME`
//! pointe toujours vers un dossier temporaire : aucun essai n'écrit dans le vrai `~/.appsmarket`,
//! et les mots de passe des clés rangés le sont dans un fichier, jamais dans le vrai trousseau
//! (`APPSMARKET_CREDENTIALS=file`). Les mots de passe passent par l'entrée standard ou par la
//! variable de Tauri, jamais par le terminal, pour qu'aucun essai n'attende une saisie. Les essais
//! des clés elles-mêmes sont dans `keys.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use am_protocol::signature::{Signature, decode_tauri};
use serde_json::Value;

/// Mot de passe public des clés de `fixtures/keys`.
const FIXTURE_PASSWORD: &str = "appsmarket-fixtures";
const PASSWORD_VAR: &str = "TAURI_SIGNING_PRIVATE_KEY_PASSWORD";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Le binaire, sans les variables de l'utilisateur qui changeraient son comportement.
fn drakbase(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_drakbase"));
    command
        .env("HOME", home)
        .env_remove("APPSMARKET_HOME")
        .env("APPSMARKET_CREDENTIALS", "file")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY")
        .env_remove(PASSWORD_VAR)
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> Output {
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn verify(home: &Path, file: &Path, pubkey: impl AsRef<std::ffi::OsStr>, version: &str) -> Output {
    run(drakbase(home)
        .arg("verify")
        .arg(file)
        .arg("--pubkey")
        .arg(pubkey)
        .args(["--version", version]))
}

#[test]
fn sign_then_verify_with_the_fixture_key() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("payload.bin");
    fs::copy(fixtures().join("files/payload.bin"), &file).unwrap();
    let output = run(drakbase(dir.path())
        .env(PASSWORD_VAR, FIXTURE_PASSWORD)
        .arg("sign")
        .arg(&file)
        .args(["--app", "test-app", "--version", "v1.2", "--key"])
        .arg(fixtures().join("keys/test-app.secret")));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("1.2.0 (forme canonique de « v1.2 »)"),
        "{}",
        stdout(&output)
    );

    // Le .sig a la forme de ceux de Tauri, avec la version canonique en plus.
    let encoded = fs::read_to_string(dir.path().join("payload.bin.sig")).unwrap();
    assert!(!encoded.ends_with('\n'));
    let text = decode_tauri(&encoded).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
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
    assert!(Signature::from_tauri(&encoded).unwrap().is_prehashed());

    let pubkey = fixtures().join("keys/test-app.pub");
    let ok = verify(dir.path(), &file, &pubkey, "1.2");
    assert!(ok.status.success(), "{}", stdout(&ok));
    for number in 1..=5 {
        assert!(
            stdout(&ok).contains(&format!("✓ {number}.")),
            "{}",
            stdout(&ok)
        );
    }
    // La clé peut aussi être donnée telle quelle.
    let content = fs::read_to_string(&pubkey).unwrap();
    assert!(
        verify(dir.path(), &file, &content, "1.2.0")
            .status
            .success()
    );

    let wrong_version = verify(dir.path(), &file, &pubkey, "1.3.0");
    assert_eq!(wrong_version.status.code(), Some(1));
    assert!(
        stdout(&wrong_version).contains("✗ 5."),
        "{}",
        stdout(&wrong_version)
    );
    let other_key = verify(
        dir.path(),
        &file,
        fixtures().join("keys/other-app.pub"),
        "1.2.0",
    );
    assert!(
        stdout(&other_key).contains("✗ 2."),
        "{}",
        stdout(&other_key)
    );
    fs::write(&file, b"autre contenu").unwrap();
    let modified = verify(dir.path(), &file, &pubkey, "1.2.0");
    assert!(stdout(&modified).contains("✗ 3."), "{}", stdout(&modified));
}

#[test]
fn sign_takes_the_key_from_tauri_variables_unless_key_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.bin");
    fs::write(&file, b"contenu").unwrap();
    let secret = fixtures().join("keys/test-app.secret");
    let content = fs::read_to_string(&secret).unwrap();
    let sign = |key: &str, password: &str| {
        run(drakbase(dir.path())
            .env("TAURI_SIGNING_PRIVATE_KEY", key)
            .env(PASSWORD_VAR, password)
            .arg("sign")
            .arg(&file)
            .args(["--app", "test-app", "--version", "2.0.0"]))
    };

    // Comme Tauri : la variable contient la clé, ou le chemin de son fichier.
    for (key, origin) in [
        (content.as_str(), "lue dans : TAURI_SIGNING_PRIVATE_KEY\n"),
        (
            secret.to_str().unwrap(),
            "lue dans : TAURI_SIGNING_PRIVATE_KEY (",
        ),
    ] {
        let output = sign(key, FIXTURE_PASSWORD);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(origin), "{}", stdout(&output));
        let checked = verify(
            dir.path(),
            &file,
            fixtures().join("keys/test-app.pub"),
            "2.0.0",
        );
        assert!(checked.status.success(), "{}", stdout(&checked));
    }

    let wrong = sign(&content, "mauvais mot de passe");
    assert_eq!(wrong.status.code(), Some(1));
    assert!(
        stderr(&wrong).contains("mot de passe faux"),
        "{}",
        stderr(&wrong)
    );

    // --key l'emporte sur la variable.
    let output = run(drakbase(dir.path())
        .env(
            "TAURI_SIGNING_PRIVATE_KEY",
            fixtures().join("keys/other-app.secret"),
        )
        .env(PASSWORD_VAR, FIXTURE_PASSWORD)
        .arg("sign")
        .arg(&file)
        .args(["--app", "test-app", "--version", "2.0.0", "--key"])
        .arg(&secret));
    assert!(output.status.success(), "{}", stderr(&output));
    let checked = verify(
        dir.path(),
        &file,
        fixtures().join("keys/test-app.pub"),
        "2.0.0",
    );
    assert!(checked.status.success(), "{}", stdout(&checked));
}

#[test]
fn sign_warns_when_the_key_is_not_one_of_the_app_keys() {
    let home = tempfile::tempdir().unwrap();
    // Dossier par défaut, $HOME/.appsmarket, où l'app « demo » a une autre clé.
    let keys = home.path().join(".appsmarket/keys");
    fs::create_dir_all(&keys).unwrap();
    fs::copy(
        fixtures().join("keys/other-app.pub"),
        keys.join("demo.key.pub"),
    )
    .unwrap();
    let file = home.path().join("a.bin");
    fs::write(&file, b"contenu").unwrap();
    let output = run(drakbase(home.path())
        .env(PASSWORD_VAR, FIXTURE_PASSWORD)
        .arg("sign")
        .arg(&file)
        .args(["--app", "demo", "--version", "1.0.0", "--key"])
        .arg(fixtures().join("keys/test-app.secret")));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ni la clé principale ni la clé de secours de l'app demo"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn sign_refuses_before_asking_for_a_password() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("a.bin");
    fs::write(&file, b"contenu").unwrap();
    let sign = |extra: &[&str]| {
        run(drakbase(home.path())
            .env(PASSWORD_VAR, FIXTURE_PASSWORD)
            .arg("sign")
            .arg(&file)
            .args(["--app", "demo"])
            .args(extra))
    };
    let public = fixtures().join("keys/test-app.pub");
    let secret = fixtures().join("keys/test-app.secret");
    for (extra, message) in [
        (vec!["--version", "1.0.0"], "drakbase keygen demo"),
        (
            vec!["--version", "1.0.0", "--key", public.to_str().unwrap()],
            "clé publique",
        ),
        (
            vec!["--version", "1.0.0+5", "--key", secret.to_str().unwrap()],
            "métadonnées de build",
        ),
    ] {
        let output = sign(&extra);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains(message), "{}", stderr(&output));
    }
    assert!(!home.path().join("a.bin.sig").exists());
}

/// Les cas communs de `fixtures/signatures.json`, par le binaire : chacun s'arrête au bon contrôle.
#[test]
fn verify_replays_the_fixture_cases() {
    let home = tempfile::tempdir().unwrap();
    let cases: Value =
        serde_json::from_str(&fs::read_to_string(fixtures().join("signatures.json")).unwrap())
            .unwrap();
    for case in cases["cases"].as_array().unwrap() {
        let field = |name: &str| case[name].as_str().unwrap();
        let output = run(drakbase(home.path())
            .arg("verify")
            .arg(fixtures().join(field("file")))
            .arg("--pubkey")
            .arg(fixtures().join(field("key")))
            .arg("--signature")
            .arg(fixtures().join(field("signature")))
            .args(["--version", field("announced_version")]));
        let report = stdout(&output);
        let failed_check = match (field("expected"), field("name")) {
            ("ok", _) => None,
            ("legacy", _) => Some(1),
            ("key_id_mismatch", _) => Some(2),
            ("invalid", "commentaire de confiance modifié") => Some(4),
            ("invalid", _) => Some(3),
            ("missing_version" | "version_mismatch", _) => Some(5),
            (other, _) => panic!("résultat inconnu : {other}"),
        };
        match failed_check {
            None => {
                assert!(output.status.success(), "{report}");
                assert!(report.contains("Signature valide."), "{report}");
            }
            Some(number) => {
                assert_eq!(output.status.code(), Some(1), "{report}");
                assert!(report.contains(&format!("✗ {number}.")), "{report}");
                assert!(report.contains("Signature refusée."), "{report}");
            }
        }
    }
}

/// Fausse app minimale : un exécutable Mach-O copié de /usr/bin/true et son Info.plist.
fn fake_app(dir: &Path, name: &str) -> PathBuf {
    let app = dir.join(format!("{name}.app"));
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::copy("/usr/bin/true", app.join("Contents/MacOS").join(name)).unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>fr.example.{name}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0.0</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
    app
}

fn codesign(args: &[&str], path: &Path) {
    let output = Command::new("codesign")
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn pack_macos_refuses_adhoc_and_unsigned_bundles() {
    let dir = tempfile::tempdir().unwrap();
    let pack = |app: &Path| run(drakbase(dir.path()).arg("pack-macos").arg(app).arg("--dmg"));

    let unsigned = fake_app(dir.path(), "Brut");
    codesign(
        &["--remove-signature"],
        &unsigned.join("Contents/MacOS/Brut"),
    );
    let output = pack(&unsigned);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("n'est pas signé"),
        "{}",
        stderr(&output)
    );

    // Signée ad hoc, comme le fait `codesign --sign -`.
    let adhoc = fake_app(dir.path(), "Demo");
    codesign(&["--sign", "-", "--force"], &adhoc);
    let output = pack(&adhoc);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("ad hoc"), "{}", stderr(&output));

    // Rien n'a été écrit à côté des bundles.
    let mut names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["Brut.app", "Demo.app"]);

    // Une archive déjà là est refusée avant tout contrôle, sauf avec --force.
    fs::write(dir.path().join("Demo-1.0.0.app.tar.gz"), "ancienne").unwrap();
    let output = pack(&adhoc);
    assert!(
        stderr(&output).contains("existe déjà"),
        "{}",
        stderr(&output)
    );

    let folder = dir.path().join("Dossier");
    fs::create_dir(&folder).unwrap();
    let output = pack(&folder);
    assert!(
        stderr(&output).contains("n'est pas un bundle d'app"),
        "{}",
        stderr(&output)
    );
}
