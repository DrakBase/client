//! Essais des clés par le binaire : `keygen`, `backup`, `restore` et `exec`, sur des bases
//! KeePassXC jetables. Les mots de passe des clés vont dans le fichier des essais, jamais dans le
//! vrai trousseau. Sans keepassxc-cli, les essais qui en ont besoin s'arrêtent après l'avoir dit.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

use am_protocol::signature::PublicKey;
use serde_json::Value;

use support::{
    FIXTURE_PASSWORD, KEEPASSXC_PASSWORD, both, drakbase, fixture_keys, fixture_keys_saved,
    keepassxc_database, keepassxc_entry, public_key, run, run_with_stdin, stderr, stdout,
};

const MAIN_ENTRY: &str = "Drakbase - Apps Market/test-app - clé principale";
const BACKUP_ENTRY: &str = "Drakbase - Apps Market/test-app - clé de secours";

fn key_id(public_key: &str) -> String {
    PublicKey::from_tauri(public_key.trim())
        .unwrap()
        .key_id()
        .to_hex()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Les mots de passe rangés, comme dans le trousseau : de l'identifiant d'une clé à son mot de
/// passe.
fn stored_passwords(home: &Path) -> Value {
    serde_json::from_str(&read(&home.join("outil/key-passwords.json"))).unwrap()
}

/// Signe un fichier avec la clé principale de l'app, sans donner de mot de passe.
fn sign(home: &Path, app: &str) -> Output {
    let file = home.join("Demo-1.0.0.app.tar.gz");
    fs::write(&file, b"archive").unwrap();
    run(drakbase(home)
        .arg("sign")
        .arg(&file)
        .args(["--app", app, "--version", "1.0.0"]))
}

#[test]
fn keygen_saves_both_keys_then_keeps_only_the_main_one_here() {
    let home = tempfile::tempdir().unwrap();
    let Some(database) = keepassxc_database(home.path()) else {
        return;
    };
    let keys = home.path().join("outil/keys");
    let keygen = |input: &str| {
        run_with_stdin(
            drakbase(home.path())
                .args(["keygen", "demo", "--password-stdin", "--keepassxc"])
                .arg(&database),
            input,
        )
    };

    // Un mauvais mot de passe de la base : rien n'est créé, ni retenu.
    let output = keygen("pas le bon\n");
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("mot de passe faux"),
        "{}",
        both(&output)
    );
    assert!(!home.path().join("outil").exists());

    let output = keygen(&format!("{KEEPASSXC_PASSWORD}\n"));
    assert!(output.status.success(), "{}", both(&output));
    assert_eq!(mode(&keys), 0o700);
    for (name, wanted) in [
        ("demo.key", 0o600),
        ("demo.key.pub", 0o644),
        ("demo.backup.key.pub", 0o644),
    ] {
        let path = keys.join(name);
        assert_eq!(mode(&path), wanted, "{name}");
        // Sans retour à la ligne final, comme Tauri, qui lit ces fichiers tels quels.
        assert!(!read(&path).ends_with('\n'), "{name}");
    }
    // La clé secrète de secours ne reste que dans KeePassXC.
    assert!(!keys.join("demo.backup.key").exists());
    let report = stdout(&output);
    let main_id = key_id(&read(&keys.join("demo.key.pub")));
    let backup_id = key_id(&read(&keys.join("demo.backup.key.pub")));
    for expected in [
        main_id.as_str(),
        backup_id.as_str(),
        "Base KeePassXC retenue",
        "« Drakbase - Apps Market/demo - clé principale » et « Drakbase - Apps Market/demo - clé de secours » écrites et relues",
        "seulement dans KeePassXC",
        "drakbase init demo",
    ] {
        assert!(report.contains(expected), "{expected}\n{report}");
    }

    // KeePassXC garde les deux clés, chacune avec son propre mot de passe ; celui de la principale
    // est aussi rangé sur ce Mac, et la sauvegarde est notée.
    let main = keepassxc_entry(&database, "Drakbase - Apps Market/demo - clé principale").unwrap();
    assert_eq!(main.username, main_id);
    assert!(
        main.notes.contains(&read(&keys.join("demo.key"))),
        "{}",
        main.notes
    );
    assert!(
        main.notes.contains("drakbase restore demo"),
        "{}",
        main.notes
    );
    let backup =
        keepassxc_entry(&database, "Drakbase - Apps Market/demo - clé de secours").unwrap();
    assert_eq!(backup.username, backup_id);
    assert_ne!(backup.password, main.password);
    let stored = stored_passwords(home.path());
    assert_eq!(stored[&main_id], Value::String(main.password.clone()));
    assert!(stored.get(&backup_id).is_none(), "{stored}");
    let records: Value =
        serde_json::from_str(&read(&home.path().join("outil/backups.json"))).unwrap();
    assert_eq!(records["demo"]["main"], Value::String(main_id.clone()));
    assert_eq!(records["demo"]["backup"], Value::String(backup_id));
    assert_eq!(
        records["demo"]["keepassxc"],
        Value::String(database.display().to_string())
    );

    // La clé principale signe sans qu'on donne son mot de passe.
    let signed = sign(home.path(), "demo");
    assert!(signed.status.success(), "{}", both(&signed));
    assert!(
        stdout(&signed).contains("clé principale de l'app demo"),
        "{}",
        both(&signed)
    );

    // Une seconde fois, avec la base retenue : refusé, sans rien toucher.
    let again = || {
        run_with_stdin(
            drakbase(home.path()).args(["keygen", "demo", "--password-stdin"]),
            &format!("{KEEPASSXC_PASSWORD}\n"),
        )
    };
    let before = fs::read(keys.join("demo.key")).unwrap();
    let output = again();
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("existent déjà"),
        "{}",
        both(&output)
    );
    assert_eq!(fs::read(keys.join("demo.key")).unwrap(), before);

    // Même sans les fichiers : KeePassXC a déjà des clés pour l'app, que restore réinstalle.
    fs::remove_dir_all(&keys).unwrap();
    let output = again();
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("« drakbase restore demo »"),
        "{}",
        both(&output)
    );
    assert!(!keys.exists());
}

#[test]
fn keygen_refuses_bad_names_and_missing_databases() {
    let home = tempfile::tempdir().unwrap();
    let output = run(drakbase(home.path()).args(["keygen", "../ailleurs", "--password-stdin"]));
    assert_eq!(output.status.code(), Some(2));
    let output = run_with_stdin(
        drakbase(home.path()).args([
            "keygen",
            "demo",
            "--password-stdin",
            "--keepassxc",
            "/nulle/part.kdbx",
        ]),
        "x\n",
    );
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(stderr(&output).contains("introuvable"), "{}", both(&output));
    assert!(!home.path().join("outil").exists());
}

#[test]
fn backup_saves_existing_keys_and_restore_brings_them_back() {
    let home = tempfile::tempdir().unwrap();
    let Some(database) = keepassxc_database(home.path()) else {
        return;
    };
    fixture_keys(home.path());
    let keys = home.path().join("outil/keys");
    let main_id = key_id(&public_key("test-app"));
    let backup_id = key_id(&public_key("other-app"));
    let backup = |home: &Path, args: &[&str], input: String| {
        run_with_stdin(
            drakbase(home)
                .args(["backup", "test-app", "--password-stdin"])
                .args(args),
            &input,
        )
    };

    // Un mot de passe de clé faux : rien n'est sauvegardé ni retiré.
    let output = backup(
        home.path(),
        &["--keepassxc", database.to_str().unwrap()],
        format!("{KEEPASSXC_PASSWORD}\npas le bon\n"),
    );
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("n'ouvre pas la clé"),
        "{}",
        both(&output)
    );
    assert!(keepassxc_entry(&database, MAIN_ENTRY).is_none());
    assert!(keys.join("test-app.backup.key").is_file());

    // Les deux mots de passe : les deux clés vont dans KeePassXC, la clé secrète de secours
    // quitte le Mac, et le mot de passe de la principale est rangé.
    let output = backup(
        home.path(),
        &[],
        format!("{KEEPASSXC_PASSWORD}\n{FIXTURE_PASSWORD}\n{FIXTURE_PASSWORD}\n"),
    );
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        format!("✓ « {MAIN_ENTRY} » écrite et relue"),
        format!("✓ « {BACKUP_ENTRY} » écrite et relue"),
        "mot de passe de la clé principale rangé".to_owned(),
        "clé secrète de secours retirée de ce Mac".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    assert!(!keys.join("test-app.backup.key").exists());
    assert!(keys.join("test-app.backup.key.pub").is_file());
    let main = keepassxc_entry(&database, MAIN_ENTRY).unwrap();
    assert_eq!(main.username, main_id);
    assert_eq!(main.password, FIXTURE_PASSWORD);
    assert_eq!(
        keepassxc_entry(&database, BACKUP_ENTRY).unwrap().username,
        backup_id
    );
    assert_eq!(
        stored_passwords(home.path())[&main_id],
        Value::String(FIXTURE_PASSWORD.to_owned())
    );
    let signed = sign(home.path(), "test-app");
    assert!(signed.status.success(), "{}", both(&signed));

    // Une seconde fois : tout est déjà là et se relit ; seule la base demande son mot de passe.
    let output = backup(home.path(), &[], format!("{KEEPASSXC_PASSWORD}\n"));
    assert!(output.status.success(), "{}", both(&output));
    assert_eq!(
        stdout(&output).matches("déjà là, relue").count(),
        2,
        "{}",
        both(&output)
    );

    // Sur un nouveau Mac : la clé principale, son mot de passe et les deux clés publiques.
    let other = tempfile::tempdir().unwrap();
    let restore = |args: &[&str]| {
        run_with_stdin(
            drakbase(other.path())
                .args(["restore", "test-app", "--password-stdin"])
                .args(args),
            &format!("{KEEPASSXC_PASSWORD}\n"),
        )
    };
    let output = restore(&["--keepassxc", database.to_str().unwrap()]);
    assert!(output.status.success(), "{}", both(&output));
    let restored = other.path().join("outil/keys");
    assert_eq!(
        read(&restored.join("test-app.key")),
        read(&keys.join("test-app.key"))
    );
    assert_eq!(mode(&restored.join("test-app.key")), 0o600);
    assert_eq!(
        read(&restored.join("test-app.key.pub")),
        public_key("test-app")
    );
    assert_eq!(
        read(&restored.join("test-app.backup.key.pub")),
        public_key("other-app")
    );
    assert!(!restored.join("test-app.backup.key").exists());
    assert_eq!(
        stored_passwords(other.path())[&main_id],
        Value::String(FIXTURE_PASSWORD.to_owned())
    );
    let signed = sign(other.path(), "test-app");
    assert!(signed.status.success(), "{}", both(&signed));

    // --backup-key ajoute la clé secrète de secours et son mot de passe.
    let output = restore(&["--backup-key"]);
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("déjà en place"),
        "{}",
        both(&output)
    );
    assert_eq!(mode(&restored.join("test-app.backup.key")), 0o600);
    assert_eq!(
        stored_passwords(other.path())[&backup_id],
        Value::String(FIXTURE_PASSWORD.to_owned())
    );

    // Une autre clé déjà là n'est jamais remplacée.
    fs::write(restored.join("test-app.key.pub"), public_key("other-app")).unwrap();
    let output = restore(&[]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("je ne remplace jamais une clé"),
        "{}",
        both(&output)
    );
}

#[test]
fn exec_hands_the_main_key_and_its_password_to_a_command() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let seen = home.path().join("variables.txt");
    let output = run(drakbase(home.path())
        .args([
            "exec",
            "test-app",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s\\n%s' \"$TAURI_SIGNING_PRIVATE_KEY\" \"$TAURI_SIGNING_PRIVATE_KEY_PASSWORD\" > \"$0\"",
        ])
        .arg(&seen));
    assert!(output.status.success(), "{}", both(&output));
    assert_eq!(
        read(&seen),
        format!(
            "{}\n{FIXTURE_PASSWORD}",
            home.path().join("outil/keys/test-app.key").display()
        )
    );
    assert!(
        stderr(&output).contains(&format!(
            "clé principale {} de l'app test-app",
            key_id(&public_key("test-app"))
        )),
        "{}",
        both(&output)
    );

    // Le code de sortie est celui de la commande.
    let output =
        run(drakbase(home.path()).args(["exec", "test-app", "--", "/bin/sh", "-c", "exit 3"]));
    assert_eq!(output.status.code(), Some(3), "{}", both(&output));

    // Un mot de passe rangé qui n'ouvre pas la clé : rien n'est lancé.
    fs::write(
        home.path().join("outil/key-passwords.json"),
        format!(
            "{{\"{}\": \"pas le bon\"}}",
            key_id(&public_key("test-app"))
        ),
    )
    .unwrap();
    let output = run(drakbase(home.path())
        .args(["exec", "test-app", "--", "/bin/sh", "-c", "touch \"$0\""])
        .arg(home.path().join("lancé")));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("« drakbase restore test-app »"),
        "{}",
        both(&output)
    );
    assert!(!home.path().join("lancé").exists());

    // Sans commande : erreur d'utilisation.
    let output = run(drakbase(home.path()).args(["exec", "test-app"]));
    assert_eq!(output.status.code(), Some(2), "{}", both(&output));
}
