//! Essais de `login`, `logout`, `publish` et `release` par le binaire, contre le faux serveur de
//! `tests/support/fake_server.rs`, qui suit docs/API.md. Les identifiants vont dans un fichier du
//! dossier temporaire de chaque essai : aucun n'écrit dans le vrai trousseau. Aucun essai ne
//! passe par le terminal, pour qu'aucun n'attende une saisie.

mod support;

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use am_protocol::signature::PublicKey;
use am_protocol::{Arch, BundleType, Channel, Os};
use serde_json::Value;
use sha2::{Digest, Sha256};

use support::fake_server::{
    FakeApp as ServerApp, FakeArtifact, FakeKey, FakeRelease, FakeServer, State,
};
use support::{
    CPU_ARM64, CPU_X86_64, ELF_AARCH64, ELF_OBJECT, FakeApp, both, drakbase, elf, fixtures, login,
    public_key, run, run_with_stdin, sign, stderr, stdout,
};

fn key_id(name: &str) -> String {
    PublicKey::from_tauri(&public_key(name))
        .unwrap()
        .key_id()
        .to_hex()
}

fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

/// La taille d'un fichier comme l'écrit l'outil : « 38 o », « 4,0 Mio ».
fn size(path: &Path) -> String {
    am_protocol::human_size(fs::metadata(path).unwrap().len())
}

/// L'app `Demo` (`fr.example.Demo`), pour arm64 seulement, sans clé embarquée.
fn demo_app(version: &str) -> FakeApp<'_> {
    FakeApp {
        name: "Demo",
        version,
        identifier: "fr.example.Demo",
        cpus: &[CPU_ARM64],
        public_keys: &[],
    }
}

/// `drakbase publish --app test-app --version <version> --channel <canal>`, puis `extra`.
fn publish(home: &Path, version: &str, channel: &str, extra: &[&str]) -> Output {
    run(drakbase(home)
        .args([
            "publish",
            "--app",
            "test-app",
            "--version",
            version,
            "--channel",
            channel,
        ])
        .args(extra))
}

fn path(file: &Path) -> &str {
    file.to_str().unwrap()
}

fn dist(home: &Path) -> PathBuf {
    let dir = home.join("dist");
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn login_checks_the_token_then_logout_forgets_it() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    let credentials = home.path().join("outil/credentials.json");

    // Un jeton refusé n'est pas rangé.
    let refused = login(home.path(), &server.url, "amk_inconnu");
    assert_eq!(refused.status.code(), Some(1), "{}", both(&refused));
    assert!(
        stderr(&refused).contains("(unauthorized)"),
        "{}",
        both(&refused)
    );
    assert!(
        stderr(&refused).contains("Rien n'a été enregistré"),
        "{}",
        both(&refused)
    );
    assert!(!credentials.exists());

    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
    let report = stdout(&output);
    for expected in [
        format!("Connecté à {}", server.url),
        "compte : Camille Essai (camille@example.fr), éditeur".to_owned(),
        "« Mac d'essai », portées releases:write, releases:publish et listing:write, pour toutes tes apps, valable jusqu'au 2027-10-25".to_owned(),
        "Adresse et jeton rangés dans le fichier".to_owned(),
    ] {
        assert!(report.contains(&expected), "{expected}\n{report}");
    }
    assert!(
        !both(&output).contains("amk_essai_complet"),
        "jeton affiché"
    );
    assert_eq!(
        fs::metadata(&credentials).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored: Value = serde_json::from_str(&fs::read_to_string(&credentials).unwrap()).unwrap();
    assert_eq!(stored["server"], server.url.as_str());
    assert_eq!(stored["token"], "amk_essai_complet");
    {
        let state = server.state();
        let whoami = state.requests.last().unwrap();
        assert_eq!(whoami.path, "/api/v1/publish/whoami");
        assert_eq!(
            whoami.header("authorization"),
            Some("Bearer amk_essai_complet")
        );
        assert!(
            whoami
                .header("user-agent")
                .is_some_and(|agent| agent.starts_with("drakbase/"))
        );
    }

    // Sans --server : l'adresse de la connexion actuelle, et un jeton sans droit de publier.
    let again = run_with_stdin(
        drakbase(home.path()).args(["login", "--token-stdin"]),
        "amk_essai_ecriture\n",
    );
    assert!(again.status.success(), "{}", both(&again));
    assert!(
        stdout(&again).contains("celui de la connexion actuelle"),
        "{}",
        stdout(&again)
    );
    assert!(
        stdout(&again).contains("laisseront les versions en brouillon"),
        "{}",
        stdout(&again)
    );

    let logout = run(drakbase(home.path()).arg("logout"));
    assert!(logout.status.success(), "{}", both(&logout));
    assert!(
        stdout(&logout).contains(&format!("Déconnecté de {}", server.url)),
        "{}",
        stdout(&logout)
    );
    assert!(stdout(&logout).contains("reste valable sur le serveur"));
    assert!(!credentials.exists());
    let twice = run(drakbase(home.path()).arg("logout"));
    assert!(twice.status.success());
    assert!(
        stdout(&twice).contains("Aucune connexion enregistrée"),
        "{}",
        stdout(&twice)
    );
}

#[test]
fn login_refuses_bad_addresses_and_tokens_before_storing_anything() {
    let home = tempfile::tempdir().unwrap();
    // Un port libéré aussitôt : la connexion y est refusée.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    for (server, token, message) in [
        ("http://maj.example.fr", "amk_x", "en clair"),
        ("https://maj.example.fr/admin", "amk_x", "sans chemin"),
        (closed.as_str(), "jeton", "commence par amk_"),
        (closed.as_str(), "amk_x", "connexion impossible"),
    ] {
        let output = login(home.path(), server, token);
        assert_eq!(output.status.code(), Some(1), "{}", both(&output));
        assert!(
            stderr(&output).contains(message),
            "{message}\n{}",
            both(&output)
        );
    }
    let output = run_with_stdin(
        drakbase(home.path()).args(["login", "--token-stdin"]),
        "amk_x\n",
    );
    assert!(stderr(&output).contains("--server"), "{}", both(&output));
    assert!(!home.path().join("outil").exists());
}

#[test]
fn publish_sends_the_signed_artifacts_then_publishes() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    server
        .state()
        .publish_version("test-app", "1.1.0", Channel::Stable);
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let embedded = [public_key("test-app"), public_key("other-app")];
    let archive = FakeApp {
        name: "Demo",
        version: "1.2",
        identifier: "fr.example.Demo",
        cpus: &[CPU_ARM64, CPU_X86_64],
        public_keys: &embedded,
    }
    .archive(&dir);
    sign(home.path(), &archive, "1.2.0", "test-app");
    let dmg = dir.join("Demo-1.2.dmg");
    fs::write(&dmg, "image disque de première installation").unwrap();
    let notes = home.path().join("notes.md");
    fs::write(&notes, "- Nouveauté\n").unwrap();

    let output = publish(
        home.path(),
        "v1.2",
        "stable",
        &[
            "--notes",
            path(&notes),
            "--publish",
            path(&archive),
            path(&dmg),
        ],
    );
    assert!(output.status.success(), "{}", both(&output));
    let report = stdout(&output);
    for expected in [
        "compte Camille Essai, jeton « Mac d'essai »".to_owned(),
        "Version : 1.2.0 (forme canonique de « v1.2 »), dernière version stable : 1.1.0".to_owned(),
        format!(
            "✓ darwin/universal/app : Demo-1.2.app.tar.gz ({}), fr.example.Demo 1.2, arm64 et x86_64, signé par la clé {} (active)",
            size(&archive),
            key_id("test-app")
        ),
        "✓ darwin/universal/dmg : Demo-1.2.dmg (38 o), sans signature (première installation)"
            .to_owned(),
        format!(
            "Clés embarquées par cette version : {}, {}",
            key_id("test-app"),
            key_id("other-app")
        ),
        "Brouillon 1.2.0 créé sur le serveur.".to_owned(),
        format!("✓ darwin/universal/app : envoyé ({})", size(&archive)),
        "✓ darwin/universal/dmg : envoyé (38 o)".to_owned(),
        "✓ Version 1.2.0 publiée sur le canal stable.".to_owned(),
        format!("darwin/universal/app : {}/dl/", server.url),
    ] {
        assert!(report.contains(&expected), "{expected}\n{report}");
    }

    let state = server.state();
    let release = &state.apps["test-app"].releases["1.2.0"];
    assert_eq!(release.status, "published");
    assert_eq!(release.channel, Channel::Stable);
    assert_eq!(release.notes_md, "- Nouveauté\n");
    assert_eq!(
        release.trusted_key_ids,
        [key_id("test-app"), key_id("other-app")]
    );
    let mut slots: Vec<String> = release
        .artifacts
        .iter()
        .map(|artifact| artifact.slot())
        .collect();
    slots.sort();
    assert_eq!(slots, ["darwin/universal/app", "darwin/universal/dmg"]);

    // Les en-têtes du contrat, et un corps de la taille annoncée, pas envoyé par morceaux.
    let uploads = state.uploads();
    assert_eq!(uploads.len(), 2);
    let app = uploads
        .iter()
        .find(|upload| upload.path.ends_with("/app"))
        .unwrap();
    assert_eq!(
        app.path,
        "/api/v1/publish/apps/test-app/releases/1.2.0/artifacts/darwin/universal/app"
    );
    let size = fs::metadata(&archive).unwrap().len().to_string();
    assert_eq!(app.header("content-length"), Some(size.as_str()));
    assert_eq!(app.header("transfer-encoding"), None);
    assert_eq!(app.header("content-type"), Some("application/octet-stream"));
    assert_eq!(
        app.header("x-appsmarket-filename"),
        Some("Demo-1.2.app.tar.gz")
    );
    assert_eq!(
        app.header("x-appsmarket-sha256"),
        Some(sha256(&archive).as_str())
    );
    let signature = fs::read_to_string(dir.join("Demo-1.2.app.tar.gz.sig")).unwrap();
    assert_eq!(app.header("x-appsmarket-signature"), Some(signature.trim()));
    let dmg_upload = uploads
        .iter()
        .find(|upload| upload.path.ends_with("/dmg"))
        .unwrap();
    assert_eq!(dmg_upload.header("x-appsmarket-signature"), None);
    assert_eq!(
        dmg_upload.header("x-appsmarket-sha256"),
        Some(sha256(&dmg).as_str())
    );
    let publish_request = state
        .requests
        .iter()
        .find(|request| request.path.ends_with("/publish"))
        .unwrap();
    assert_eq!(publish_request.method, "POST");
    assert_eq!(
        publish_request.header("content-type"),
        Some("application/json")
    );
}

#[test]
fn publish_refuses_a_version_that_is_not_higher_unless_forced() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    server
        .state()
        .publish_version("test-app", "1.2.0", Channel::Stable);
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let archive = demo_app("1.1.5").archive(&dist(home.path()));
    sign(home.path(), &archive, "1.1.5", "test-app");

    let refused = publish(home.path(), "1.1.5", "stable", &[path(&archive)]);
    assert_eq!(refused.status.code(), Some(1), "{}", both(&refused));
    assert!(
        stderr(&refused).contains(
            "la version 1.1.5 n'est pas plus haute que la dernière version stable publiée, 1.2.0"
        ),
        "{}",
        both(&refused)
    );
    assert!(stderr(&refused).contains("--force"), "{}", both(&refused));
    assert!(
        !server.state().apps["test-app"]
            .releases
            .contains_key("1.1.5")
    );
    assert!(server.state().uploads().is_empty());

    let forced = publish(home.path(), "1.1.5", "stable", &["--force", path(&archive)]);
    assert!(forced.status.success(), "{}", both(&forced));
    assert!(
        stderr(&forced).contains("attention : --force"),
        "{}",
        both(&forced)
    );
    assert!(
        stdout(&forced).contains("Brouillon 1.1.5 prêt"),
        "{}",
        stdout(&forced)
    );

    // Une bêta publiée ne repart pas en stable : son canal se change dans l'administration.
    server
        .state()
        .publish_version("test-app", "1.3.0", Channel::Beta);
    let beta = demo_app("1.3.0").archive(&dist(home.path()));
    sign(home.path(), &beta, "1.3.0", "test-app");
    let promoted = publish(home.path(), "1.3.0", "stable", &["--force", path(&beta)]);
    assert_eq!(promoted.status.code(), Some(1), "{}", both(&promoted));
    assert!(
        stderr(&promoted).contains(
            "la version 1.3.0 est déjà publiée en bêta : pour la passer en stable, change son canal dans l'administration"
        ),
        "{}",
        both(&promoted)
    );
    let state = server.state();
    let draft = &state.apps["test-app"].releases["1.1.5"];
    assert_eq!(draft.status, "draft");
    assert_eq!(draft.artifacts.len(), 1);
}

#[test]
fn publish_checks_everything_it_can_before_sending_anything() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("1.3.0").archive(&dir);
    let refused = |version: &str, artifacts: &[&str], message: &str| {
        let output = publish(home.path(), version, "beta", artifacts);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{message}\n{}",
            both(&output)
        );
        assert!(
            stderr(&output).contains(message),
            "{message}\n{}",
            both(&output)
        );
    };

    refused("1.3.0", &[path(&archive)], "introuvable");
    sign(home.path(), &archive, "1.3.0", "other-app");
    refused(
        "1.3.0",
        &[path(&archive)],
        &format!(
            "signé par la clé {}, que le serveur ne connaît pas pour l'app test-app (clés déclarées : {} active)",
            key_id("other-app"),
            key_id("test-app")
        ),
    );
    sign(home.path(), &archive, "1.3.1", "test-app");
    refused(
        "1.3.0",
        &[path(&archive)],
        "est signé pour la version 1.3.1, pas 1.3.0",
    );
    sign(home.path(), &archive, "1.3.0", "test-app");
    // Refaite après la signature, avec un autre contenu.
    FakeApp {
        cpus: &[CPU_X86_64],
        ..demo_app("1.3.0")
    }
    .archive(&dir);
    refused("1.3.0", &[path(&archive)], "signature invalide");
    refused(
        "1.4.0",
        &[path(&archive)],
        "contient la version 1.3.0, pas 1.4.0",
    );
    refused(
        "1.3.0",
        &[&format!("darwin/aarch64/app={}", path(&archive))],
        "l'exécutable contient x86_64, il ne convient pas à l'arch aarch64",
    );

    let other_dir = home.path().join("autre");
    fs::create_dir(&other_dir).unwrap();
    let other = FakeApp {
        identifier: "fr.example.Autre",
        ..demo_app("1.3.0")
    }
    .archive(&other_dir);
    sign(home.path(), &other, "1.3.0", "test-app");
    refused(
        "1.3.0",
        &[path(&other)],
        "a l'identifiant fr.example.Autre, alors que l'app test-app attend fr.example.Demo",
    );

    let dmg = dir.join("Installeur.dmg");
    fs::write(&dmg, "image").unwrap();
    refused(
        "1.3.0",
        &[path(&dmg)],
        "impossible de déduire l'architecture de « Installeur.dmg »",
    );
    let exe = dir.join("Demo_1.3.0.exe");
    fs::write(&exe, "installeur").unwrap();
    refused("1.3.0", &[path(&exe)], "windows/x86_64/nsis=Demo_1.3.0.exe");
    refused(
        "1.3.0",
        &[path(&other), path(&other)],
        "vont tous deux à l'emplacement darwin/aarch64/app",
    );
    refused(
        "1.3.0",
        &[&format!("{}.sig", path(&other))],
        "est une signature",
    );
    let bundle = dir.join("Demo.app");
    fs::create_dir(&bundle).unwrap();
    refused("1.3.0", &[path(&bundle)], "drakbase release");

    // Ni brouillon ni envoi : seuls whoami et la lecture de l'app ont pu partir.
    let state = server.state();
    assert!(state.apps["test-app"].releases.is_empty());
    assert!(
        state.requests.iter().all(|request| request.method == "GET"),
        "{:?}",
        state
            .requests
            .iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>()
    );
}

#[test]
fn publish_resumes_where_it_stopped_after_a_cut_connection() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("2.0.0").archive(&dir);
    sign(home.path(), &archive, "2.0.0", "test-app");
    // Assez gros pour que la coupure tombe au milieu de l'envoi.
    let dmg = dir.join("Demo-2.0.0.dmg");
    fs::write(&dmg, vec![7u8; 4 * 1024 * 1024]).unwrap();
    server.state().cut_upload = Some(("darwin/aarch64/dmg".to_owned(), 1024 * 1024));
    let artifacts = ["--publish", path(&archive), path(&dmg)];

    let cut = publish(home.path(), "2.0.0", "stable", &artifacts);
    assert_eq!(cut.status.code(), Some(1), "{}", both(&cut));
    let message = stderr(&cut);
    for expected in [
        "erreur : envoi de darwin/aarch64/dmg (Demo-2.0.0.dmg)",
        "Le brouillon 2.0.0 reste sur le serveur",
        "Relance la même commande pour reprendre",
    ] {
        assert!(message.contains(expected), "{expected}\n{}", both(&cut));
    }
    assert!(
        stdout(&cut).contains("✓ darwin/aarch64/app : envoyé"),
        "{}",
        stdout(&cut)
    );
    {
        let state = server.state();
        let draft = &state.apps["test-app"].releases["2.0.0"];
        assert_eq!(draft.status, "draft");
        assert_eq!(draft.artifacts.len(), 1);
        assert_eq!(state.uploads().len(), 2);
    }

    let resumed = publish(home.path(), "2.0.0", "stable", &artifacts);
    assert!(resumed.status.success(), "{}", both(&resumed));
    let report = stdout(&resumed);
    for expected in [
        "Brouillon 2.0.0 repris : il était déjà sur le serveur.",
        "· darwin/aarch64/app : déjà sur le serveur, identique, pas renvoyé",
        "✓ darwin/aarch64/dmg : envoyé (4,0 Mio)",
        "✓ Version 2.0.0 publiée sur le canal stable.",
    ] {
        assert!(report.contains(expected), "{expected}\n{report}");
    }
    let state = server.state();
    assert_eq!(
        state.uploads().len(),
        3,
        "l'archive n'est envoyée qu'une fois"
    );
    assert_eq!(state.apps["test-app"].releases["2.0.0"].artifacts.len(), 2);
}

/// Un brouillon repris sans `--notes` garde ses notes : l'outil n'envoie alors pas `notes_md`, que
/// le serveur prendrait pour de nouvelles notes, vides. Avec `--notes`, elles sont remplacées.
#[test]
fn a_resumed_draft_keeps_its_notes_unless_new_ones_are_given() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("2.0.0").archive(&dir);
    sign(home.path(), &archive, "2.0.0", "test-app");
    let notes = home.path().join("notes.md");
    let draft_notes = || {
        server.state().apps["test-app"].releases["2.0.0"]
            .notes_md
            .clone()
    };
    // Ce que l'outil a demandé au serveur la dernière fois, pour les notes.
    let sent_notes = || {
        let state = server.state();
        state
            .release_bodies
            .last()
            .unwrap()
            .get("notes_md")
            .cloned()
    };
    let send = |extra: &[&str]| {
        let mut arguments = extra.to_vec();
        arguments.push(path(&archive));
        let output = publish(home.path(), "2.0.0", "stable", &arguments);
        assert!(output.status.success(), "{}", both(&output));
        output
    };

    // Le brouillon, avec ses notes.
    fs::write(&notes, "- Première rédaction\n").unwrap();
    let created = send(&["--notes", path(&notes)]);
    assert!(
        stdout(&created).contains("Brouillon 2.0.0 créé sur le serveur."),
        "{}",
        both(&created)
    );
    assert_eq!(sent_notes(), Some(Value::from("- Première rédaction\n")));
    assert_eq!(draft_notes(), "- Première rédaction\n");

    // Repris sans --notes : aucune note n'est envoyée, et le brouillon garde les siennes.
    let resumed = send(&[]);
    assert!(
        stdout(&resumed).contains("Brouillon 2.0.0 repris : il était déjà sur le serveur."),
        "{}",
        both(&resumed)
    );
    assert_eq!(
        sent_notes(),
        None,
        "notes_md ne doit pas partir sans --notes"
    );
    assert_eq!(draft_notes(), "- Première rédaction\n");

    // Avec --notes, elles sont remplacées, même par un fichier vide.
    let empty = home.path().join("vide.md");
    fs::write(&empty, "").unwrap();
    send(&["--notes", path(&empty)]);
    assert_eq!(sent_notes(), Some(Value::from("")));
    assert_eq!(draft_notes(), "");
    fs::write(&notes, "- Seconde rédaction\n").unwrap();
    send(&["--notes", path(&notes)]);
    assert_eq!(draft_notes(), "- Seconde rédaction\n");

    // La publication, sans --notes elle non plus : la version sort avec les notes du brouillon.
    let published = send(&["--publish"]);
    assert!(
        stdout(&published).contains("✓ Version 2.0.0 publiée sur le canal stable."),
        "{}",
        both(&published)
    );
    assert_eq!(sent_notes(), None);
    let state = server.state();
    let release = &state.apps["test-app"].releases["2.0.0"];
    assert_eq!(release.status, "published");
    assert_eq!(release.notes_md, "- Seconde rédaction\n");
    // La création du brouillon suffit à le mettre à jour : une seule archive envoyée.
    assert_eq!(state.uploads().len(), 1);
}

#[test]
fn publish_shows_the_refusals_of_the_server_with_their_code() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("3.0.0").archive(&dir);
    sign(home.path(), &archive, "3.0.0", "test-app");

    server.state().reject_next_upload = Some((
        422,
        "artifact_invalid".to_owned(),
        "CFBundleIdentifier inattendu".to_owned(),
    ));
    let output = publish(home.path(), "3.0.0", "stable", &[path(&archive)]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("envoi de darwin/aarch64/app (Demo-3.0.0.app.tar.gz) : CFBundleIdentifier inattendu (artifact_invalid)"),
        "{}",
        both(&output)
    );
    assert!(stderr(&output).contains("reste sur le serveur"));

    // Le serveur annonce sa taille maximale : un fichier trop gros est refusé avant tout envoi.
    let big = dir.join("Demo-3.0.0.dmg");
    fs::write(&big, vec![1u8; 8 * 1024 * 1024]).unwrap();
    server.state().max_artifact_bytes = 1024 * 1024;
    let sent = server.state().uploads().len();
    let big_dmg = format!("darwin/universal/dmg={}", path(&big));
    let output = publish(home.path(), "3.0.0", "stable", &[&big_dmg]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains(&format!(
            "{} fait 8,0 Mio, plus que la taille maximale d'un artefact sur le serveur (1,0 Mio)",
            path(&big)
        )),
        "{}",
        both(&output)
    );
    assert_eq!(server.state().uploads().len(), sent, "rien n'est parti");

    // Un refus qui tombe avant la fin de l'envoi, la limite d'un proxy par exemple : la réponse
    // arrive malgré l'envoi en cours, avec son code et le conseil qui va avec.
    {
        let mut state = server.state();
        state.max_artifact_bytes = 1024 * 1024 * 1024;
        state.refuse_next_upload = Some((
            413,
            "payload_too_large".to_owned(),
            "corps trop gros".to_owned(),
        ));
    }
    let output = publish(home.path(), "3.0.0", "stable", &[&big_dmg]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stderr(&output).contains("(payload_too_large)"),
        "{}",
        both(&output)
    );
    assert!(
        stderr(&output).contains("AM_MAX_ARTIFACT_BYTES"),
        "{}",
        both(&output)
    );

    // Jeton révoqué depuis la connexion.
    server
        .state()
        .tokens
        .retain(|token| token.secret != "amk_essai_complet");
    let output = publish(home.path(), "3.0.0", "stable", &[path(&archive)]);
    assert!(
        stderr(&output).contains("vérification du jeton auprès de"),
        "{}",
        both(&output)
    );
    assert!(
        stderr(&output).contains("(unauthorized)\nLe jeton est refusé"),
        "{}",
        both(&output)
    );
}

#[test]
fn publish_needs_a_token_with_the_right_scope_for_the_app() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    let archive = demo_app("1.0.0").archive(&dist(home.path()));
    sign(home.path(), &archive, "1.0.0", "test-app");

    let output = publish(home.path(), "1.0.0", "stable", &[path(&archive)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("connecte-toi d'abord avec « drakbase login »"),
        "{}",
        both(&output)
    );

    assert!(
        login(home.path(), &server.url, "amk_essai_ecriture")
            .status
            .success()
    );
    let output = publish(
        home.path(),
        "1.0.0",
        "stable",
        &["--publish", path(&archive)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("n'a pas la portée releases:publish"),
        "{}",
        both(&output)
    );
    assert!(server.state().apps["test-app"].releases.is_empty());
    let output = publish(home.path(), "1.0.0", "stable", &[path(&archive)]);
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("Brouillon 1.0.0 prêt"),
        "{}",
        stdout(&output)
    );

    assert!(
        login(home.path(), &server.url, "amk_essai_autre_app")
            .status
            .success()
    );
    let output = publish(home.path(), "1.0.0", "stable", &[path(&archive)]);
    assert!(
        stderr(&output).contains("ne sert qu'à l'app autre, pas à test-app"),
        "{}",
        both(&output)
    );
}

#[test]
fn publish_finds_the_slots_of_tauri_bundles_from_their_names() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let mut artifacts = vec!["--publish".to_owned()];
    for (name, signed) in [
        ("Demo_0.3.0_x64-setup.exe", true),
        ("demo_0.3.0_amd64.AppImage", true),
        ("demo_0.3.0_arm64.deb", true),
        ("Demo_0.3.0_aarch64.dmg", false),
    ] {
        let file = dir.join(name);
        fs::write(&file, format!("contenu de {name}")).unwrap();
        if signed {
            sign(home.path(), &file, "0.3.0", "test-app");
        }
        artifacts.push(path(&file).to_owned());
    }
    let extra: Vec<&str> = artifacts.iter().map(String::as_str).collect();
    let output = publish(home.path(), "0.3.0", "beta", &extra);
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("✓ Version 0.3.0 publiée sur le canal bêta."),
        "{}",
        stdout(&output)
    );
    let state = server.state();
    let mut slots: Vec<String> = state.apps["test-app"].releases["0.3.0"]
        .artifacts
        .iter()
        .map(|artifact| artifact.slot())
        .collect();
    slots.sort();
    assert_eq!(
        slots,
        [
            "darwin/aarch64/dmg",
            "linux/aarch64/deb",
            "linux/x86_64/appimage",
            "windows/x86_64/nsis"
        ]
    );
}

/// Un projet Tauri dont `src-tauri/tauri.conf.json` embarque cette clé publique.
fn tauri_project(home: &Path, pubkey: &str) -> PathBuf {
    let project = home.join("projet");
    let config = project.join("src-tauri/tauri.conf.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let contents = serde_json::json!({
        "productName": "Demo",
        "identifier": "fr.example.demo",
        "plugins": { "updater": { "pubkey": pubkey } }
    });
    fs::write(&config, contents.to_string()).unwrap();
    project
}

/// Un artefact Tauri, signé par la clé de l'app pour cette version.
fn tauri_artifact(home: &Path, name: &str, version: &str) -> String {
    let file = dist(home).join(name);
    fs::write(&file, format!("contenu de {name}")).unwrap();
    sign(home, &file, version, "test-app");
    path(&file).to_owned()
}

#[test]
fn publish_declares_the_key_that_a_tauri_app_embeds() {
    let server = FakeServer::start(State::demo(None));
    server.state().apps.get_mut("test-app").unwrap().kind = "tauri".to_owned();
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let project = tauri_project(home.path(), &public_key("test-app"));

    // Depuis le dossier du projet, la configuration se trouve seule.
    let output = run(drakbase(home.path())
        .current_dir(&project)
        .args([
            "publish",
            "--app",
            "test-app",
            "--version",
            "0.4.0",
            "--channel",
            "stable",
            "--publish",
        ])
        .arg(tauri_artifact(
            home.path(),
            "demo_0.4.0_amd64.AppImage",
            "0.4.0",
        ))
        .arg(tauri_artifact(
            home.path(),
            "Demo_0.4.0_x64-setup.exe",
            "0.4.0",
        )));
    assert!(output.status.success(), "{}", both(&output));
    let expected = format!(
        "Clé embarquée par cette version : {} (d'après src-tauri/tauri.conf.json)",
        key_id("test-app")
    );
    assert!(
        stdout(&output).contains(&expected),
        "{expected}\n{}",
        stdout(&output)
    );
    for absent in ["qui la signe", "n'est pas déclarée sur le serveur"] {
        assert!(!stderr(&output).contains(absent), "{}", stderr(&output));
    }
    assert_eq!(
        server.state().apps["test-app"].releases["0.4.0"].trusted_key_ids,
        [key_id("test-app")]
    );

    // Ailleurs, avec --tauri-config : un fichier de plateforme qui remplace la clé l'emporte pour
    // ses artefacts. Ici, la nouvelle clé d'une rotation, que le serveur ne connaît pas encore.
    let linux = project.join("src-tauri/tauri.linux.conf.json");
    let replaced =
        serde_json::json!({ "plugins": { "updater": { "pubkey": public_key("other-app") } } });
    fs::write(&linux, replaced.to_string()).unwrap();
    let appimage = tauri_artifact(home.path(), "demo_0.5.0_amd64.AppImage", "0.5.0");
    let output = publish(
        home.path(),
        "0.5.0",
        "stable",
        &["--tauri-config", path(&project), "--publish", &appimage],
    );
    assert!(output.status.success(), "{}", both(&output));
    let expected = format!(
        "Clé embarquée par cette version : {} (d'après {})",
        key_id("other-app"),
        linux.display()
    );
    assert!(
        stdout(&output).contains(&expected),
        "{expected}\n{}",
        stdout(&output)
    );
    for warning in [
        format!("n'embarque pas la clé {} qui la signe", key_id("test-app")),
        format!(
            "la clé embarquée {} n'est pas déclarée sur le serveur",
            key_id("other-app")
        ),
    ] {
        assert!(
            stderr(&output).contains(&warning),
            "{warning}\n{}",
            stderr(&output)
        );
    }
    assert_eq!(
        server.state().apps["test-app"].releases["0.5.0"].trusted_key_ids,
        [key_id("other-app")]
    );
}

#[test]
fn publish_needs_the_configuration_of_a_tauri_app_to_publish_it() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let project = tauri_project(home.path(), &public_key("test-app"));
    let appimage = tauri_artifact(home.path(), "demo_0.4.0_amd64.AppImage", "0.4.0");
    let in_home = |extra: &[&str]| {
        run(drakbase(home.path())
            .current_dir(home.path())
            .args([
                "publish",
                "--app",
                "test-app",
                "--version",
                "0.4.0",
                "--channel",
                "stable",
            ])
            .args(extra))
    };

    // Une app Swift lit ses clés dans l'Info.plist : --tauri-config n'a pas de sens.
    let swift = in_home(&["--tauri-config", path(&project), &appimage]);
    assert_eq!(swift.status.code(), Some(1), "{}", both(&swift));
    assert!(
        stderr(&swift).contains("ne sert qu'aux apps Tauri"),
        "{}",
        both(&swift)
    );

    // Hors du projet, sans --tauri-config, rien n'est envoyé.
    server.state().apps.get_mut("test-app").unwrap().kind = "tauri".to_owned();
    let refused = in_home(&["--publish", &appimage]);
    assert_eq!(refused.status.code(), Some(1), "{}", both(&refused));
    assert!(
        stderr(&refused).contains("donne sa configuration avec --tauri-config"),
        "{}",
        both(&refused)
    );
    assert!(
        server
            .state()
            .requests
            .iter()
            .all(|request| request.method == "GET"),
        "rien ne devait être envoyé"
    );

    // Sans --publish, le brouillon se crée sans clé déclarée, à compléter dans l'administration.
    let draft = in_home(&[&appimage]);
    assert!(draft.status.success(), "{}", both(&draft));
    assert!(
        stderr(&draft).contains("coche dans l'administration la clé qu'il embarque"),
        "{}",
        stderr(&draft)
    );
    let state = server.state();
    let release = &state.apps["test-app"].releases["0.4.0"];
    assert_eq!(release.status, "draft");
    assert!(release.trusted_key_ids.is_empty());
}

#[test]
fn release_refuses_an_adhoc_bundle_before_contacting_the_server() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let requests = server.state().requests.len();

    let app = home.path().join("Demo.app");
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::copy("/usr/bin/true", app.join("Contents/MacOS/Demo")).unwrap();
    fs::write(
        app.join("Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Demo</string>
<key>CFBundleIdentifier</key><string>fr.example.Demo</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0.0</string>
</dict></plist>
"#,
    )
    .unwrap();
    let signed = Command::new("codesign")
        .args(["--sign", "-", "--force"])
        .arg(&app)
        .output()
        .unwrap();
    assert!(signed.status.success(), "{}", both(&signed));

    let output = run(drakbase(home.path())
        .env(
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
            support::FIXTURE_PASSWORD,
        )
        .arg("release")
        .arg(&app)
        .args(["--app", "test-app", "--channel", "stable", "--key"])
        .arg(fixtures().join("keys/test-app.secret")));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(stderr(&output).contains("ad hoc"), "{}", both(&output));
    assert_eq!(server.state().requests.len(), requests);
    let mut names: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["Demo.app", "outil"]);
}

#[test]
fn help_is_in_french() {
    let home = tempfile::tempdir().unwrap();
    for (command, expected) in [
        ("login", "Options :"),
        ("publish", "Arguments :"),
        ("release", "Arguments :"),
    ] {
        let output = run(drakbase(home.path()).args([command, "--help"]));
        assert!(output.status.success(), "{}", both(&output));
        let help = stdout(&output);
        assert!(help.contains("Utilisation : drakbase"), "{help}");
        assert!(help.contains(expected), "{help}");
        if command == "login" {
            assert!(!help.contains("Arguments :"), "{help}");
        }
    }
}

#[test]
fn publish_with_prune_removes_what_the_draft_should_no_longer_hold() {
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("2.1.0").archive(&dir);
    sign(home.path(), &archive, "2.1.0", "test-app");
    let dmg = dir.join("Demo-2.1.0.dmg");
    fs::write(&dmg, vec![7u8; 1024]).unwrap();

    // Le DMG part avec l'archive, par erreur.
    let first = publish(
        home.path(),
        "2.1.0",
        "stable",
        &[path(&archive), path(&dmg)],
    );
    assert!(first.status.success(), "{}", both(&first));

    // Sans --prune, le brouillon le garde, et l'outil le dit.
    let kept = publish(home.path(), "2.1.0", "stable", &[path(&archive)]);
    assert!(kept.status.success(), "{}", both(&kept));
    assert!(
        stdout(&kept).contains(
            "Le brouillon contient aussi, envoyés avant : darwin/aarch64/dmg. Relance avec --prune pour les en retirer."
        ),
        "{}",
        both(&kept)
    );
    assert_eq!(
        server.state().apps["test-app"].releases["2.1.0"]
            .artifacts
            .len(),
        2
    );

    // Avec --prune, il ne reste que l'archive, puis la version se publie.
    let pruned = publish(
        home.path(),
        "2.1.0",
        "stable",
        &["--prune", "--publish", path(&archive)],
    );
    assert!(pruned.status.success(), "{}", both(&pruned));
    let report = stdout(&pruned);
    for expected in [
        "· darwin/aarch64/app : déjà sur le serveur, identique, pas renvoyé",
        "✓ darwin/aarch64/dmg : retiré du brouillon",
        "✓ Version 2.1.0 publiée sur le canal stable.",
    ] {
        assert!(report.contains(expected), "{expected}\n{report}");
    }
    let state = server.state();
    let release = &state.apps["test-app"].releases["2.1.0"];
    assert_eq!(release.status, "published");
    let slots: Vec<String> = release
        .artifacts
        .iter()
        .map(|artifact| artifact.slot())
        .collect();
    assert_eq!(slots, ["darwin/aarch64/app"]);
    let deletions: Vec<&str> = state
        .requests
        .iter()
        .filter(|request| request.method == "DELETE")
        .map(|request| request.path.as_str())
        .collect();
    assert_eq!(deletions.len(), 1);
    assert!(
        deletions[0].starts_with("/api/v1/publish/apps/test-app/artifacts/"),
        "{deletions:?}"
    );
    assert_eq!(
        state
            .requests
            .iter()
            .find(|request| request.method == "DELETE")
            .and_then(|request| request.header("authorization")),
        Some("Bearer amk_essai_complet")
    );
}

/// La clé de secours d'essai, déclarée comme clé suivante de `test-app`.
fn declare_next_key(state: &mut State) {
    state.apps.get_mut("test-app").unwrap().keys.push(FakeKey {
        public_key: public_key("other-app"),
        status: "next".to_owned(),
    });
}

/// Un fichier déjà dans le brouillon ne repart pas, sauf si sa signature a changé : signé de
/// nouveau par une autre clé, il est renvoyé, et la version se publie avec la nouvelle signature.
#[test]
fn a_resumed_draft_takes_the_new_signature_of_an_unchanged_file() {
    let mut state = State::demo(Some("fr.example.Demo"));
    declare_next_key(&mut state);
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let dir = dist(home.path());
    let archive = demo_app("2.2.0").archive(&dir);
    let dmg = dir.join("Demo-2.2.0.dmg");
    fs::write(&dmg, "image disque").unwrap();
    let signature = || fs::read_to_string(dir.join("Demo-2.2.0.app.tar.gz.sig")).unwrap();
    let app_artifact = || {
        let state = server.state();
        let artifact = state.apps["test-app"].releases["2.2.0"]
            .artifacts
            .iter()
            .find(|artifact| artifact.bundle_type == BundleType::App)
            .unwrap();
        (
            artifact.signed_key_id.clone(),
            artifact.signed_version.clone(),
        )
    };

    // Le brouillon, signé par la clé suivante.
    sign(home.path(), &archive, "2.2.0", "other-app");
    let first = publish(
        home.path(),
        "2.2.0",
        "stable",
        &[path(&archive), path(&dmg)],
    );
    assert!(first.status.success(), "{}", both(&first));
    let expected = format!("signé par la clé {} (suivante)", key_id("other-app"));
    assert!(
        stdout(&first).contains(&expected),
        "{expected}\n{}",
        both(&first)
    );
    assert_eq!(
        app_artifact(),
        (Some(key_id("other-app")), Some("2.2.0".to_owned()))
    );
    let next_signature = signature();

    // Le même fichier, signé par la clé active : la même commande le renvoie, pas le dmg.
    sign(home.path(), &archive, "2.2.0", "test-app");
    assert_ne!(signature(), next_signature);
    let second = publish(
        home.path(),
        "2.2.0",
        "stable",
        &["--publish", path(&archive), path(&dmg)],
    );
    assert!(second.status.success(), "{}", both(&second));
    let report = stdout(&second);
    for expected in [
        format!("signé par la clé {} (active)", key_id("test-app")),
        format!(
            "✓ darwin/aarch64/app : envoyé ({}), à la place de celui du brouillon",
            size(&archive)
        ),
        "· darwin/aarch64/dmg : déjà sur le serveur, identique, pas renvoyé".to_owned(),
        "✓ Version 2.2.0 publiée sur le canal stable.".to_owned(),
    ] {
        assert!(report.contains(&expected), "{expected}\n{report}");
    }
    assert_eq!(
        app_artifact(),
        (Some(key_id("test-app")), Some("2.2.0".to_owned()))
    );
    let state = server.state();
    let uploads = state.uploads();
    assert_eq!(uploads.len(), 3, "l'archive deux fois, le dmg une seule");
    assert_eq!(
        uploads[2].header("x-appsmarket-signature"),
        Some(signature().trim())
    );
    assert_eq!(state.apps["test-app"].releases["2.2.0"].status, "published");
}

/// Un fichier ELF qui n'est pas un exécutable, un fichier objet par exemple : le serveur le
/// refuserait une fois reçu, l'outil le refuse avant de le contacter.
#[test]
fn publish_refuses_an_elf_that_is_not_an_executable_before_sending_it() {
    let mut state = State::demo(None);
    state.apps.get_mut("test-app").unwrap().kind = "cli".to_owned();
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    assert!(
        login(home.path(), &server.url, "amk_essai_complet")
            .status
            .success()
    );
    let requests = server.state().requests.len();
    let object = dist(home.path()).join("agent.o");
    fs::write(&object, elf(ELF_OBJECT, ELF_AARCH64)).unwrap();
    sign(home.path(), &object, "0.4.0", "test-app");

    for artifact in [
        path(&object).to_owned(),
        format!("linux/aarch64/bin={}", path(&object)),
    ] {
        let output = publish(home.path(), "0.4.0", "stable", &[&artifact]);
        assert_eq!(output.status.code(), Some(1), "{}", both(&output));
        let expected = format!(
            "{} est un fichier ELF qui n'est pas un exécutable",
            path(&object)
        );
        assert!(
            stderr(&output).contains(&expected),
            "{expected}\n{}",
            both(&output)
        );
    }
    assert_eq!(server.state().requests.len(), requests, "aucune requête");
}

/// Ce que le serveur contrôle encore à la publication, sur tous les artefacts du brouillon, ceux
/// des envois précédents compris : une seule clé, encore active ou suivante, et la version signée.
#[test]
fn the_server_refuses_to_publish_a_draft_whose_signatures_no_longer_fit() {
    let main = key_id("test-app");
    let other = key_id("other-app");
    let mut both_keys = [main.clone(), other.clone()];
    both_keys.sort();
    // Un artefact d'un autre emplacement, comme l'aurait laissé un envoi précédent.
    let earlier = |signed_key_id: &str, signed_version: &str| FakeArtifact {
        id: "01ESSAI0000000000000000000".to_owned(),
        os: Os::Darwin,
        arch: Arch::X86_64,
        bundle_type: BundleType::App,
        filename: "Demo-x86_64.app.tar.gz".to_owned(),
        size: 4,
        sha256: "ab".repeat(32),
        signed_key_id: Some(signed_key_id.to_owned()),
        signed_version: Some(signed_version.to_owned()),
    };
    fn draft(app: &mut ServerApp) -> &mut FakeRelease {
        app.releases.get_mut("3.1.0").unwrap()
    }
    type Change<'a> = Box<dyn Fn(&mut ServerApp) + 'a>;
    let cases: Vec<(Change<'_>, String)> = vec![
        (
            // La clé est retirée entre l'envoi et la publication.
            Box::new(|app| app.keys[0].status = "retired".to_owned()),
            format!(
                "la clé {main}, qui signe les artefacts, n'est ni active ni suivante : renvoie des artefacts signés avec la clé active (conflict)"
            ),
        ),
        (
            // La clé n'est plus déclarée du tout.
            Box::new(|app| {
                app.keys.remove(0);
            }),
            format!("la clé {main}, qui signe les artefacts, n'est ni active ni suivante"),
        ),
        (
            Box::new(|app| draft(app).artifacts.push(earlier(&other, "3.1.0"))),
            format!(
                "les artefacts sont signés par plusieurs clés ({}) : une seule est permise (conflict)",
                both_keys.join(", ")
            ),
        ),
        (
            Box::new(|app| draft(app).artifacts.push(earlier(&main, "3.0.9"))),
            "l'artefact 01ESSAI0000000000000000000 n'est pas signé pour la version 3.1.0 (conflict)"
                .to_owned(),
        ),
    ];
    for (change, expected) in cases {
        let mut state = State::demo(Some("fr.example.Demo"));
        declare_next_key(&mut state);
        let server = FakeServer::start(state);
        let home = tempfile::tempdir().unwrap();
        assert!(
            login(home.path(), &server.url, "amk_essai_complet")
                .status
                .success()
        );
        let dir = dist(home.path());
        let archive = demo_app("3.1.0").archive(&dir);
        sign(home.path(), &archive, "3.1.0", "test-app");
        let sent = publish(home.path(), "3.1.0", "stable", &[path(&archive)]);
        assert!(sent.status.success(), "{}", both(&sent));

        change(server.state().apps.get_mut("test-app").unwrap());
        // La publication, avec le dmg seul : l'archive du brouillon n'est pas relue par l'outil.
        let dmg = dir.join("Demo-3.1.0.dmg");
        fs::write(&dmg, "image disque").unwrap();
        let dmg = format!("darwin/aarch64/dmg={}", path(&dmg));
        let output = publish(home.path(), "3.1.0", "stable", &["--publish", &dmg]);
        assert_eq!(output.status.code(), Some(1), "{}", both(&output));
        for expected in [
            format!("publication de la version 3.1.0 : {expected}"),
            "Le brouillon 3.1.0 reste sur le serveur".to_owned(),
        ] {
            assert!(
                stderr(&output).contains(&expected),
                "{expected}\n{}",
                both(&output)
            );
        }
        assert_eq!(
            server.state().apps["test-app"].releases["3.1.0"].status,
            "draft"
        );
    }
}
