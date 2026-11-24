//! Essais du binaire `drakbase-stack` contre le faux serveur et un faux Docker, sur une machine
//! simulée dans un dossier temporaire (voir `support`). Ils suivent docs/PROTOCOLE.md, « Agent
//! `drakbase-stack` » : installation, mise à jour en dix étapes, permissions, échecs `safe` et
//! `manual`, versions écartées, `rollback`, signatures, archives piégées et prérequis ; puis
//! « Mises à jour validées dans l'app » : dossier de contrôle, demandes et unité des demandes.

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use support::fake_server::FakeServer;
use support::{APP, Machine, Release, arch, fixtures, key};

/// Une machine où « demo » 1.0.0 est installée, journaux vidés.
fn installed() -> (Machine, FakeServer) {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::new("1.0.0"));
    machine.install(&server);
    machine.clear_logs();
    (machine, server)
}

/// Une machine où « demo » 1.0.0 est installée avec `app_control` : l'app propose ses mises à
/// jour dans son interface, et a donc un dossier de contrôle. Journaux vidés.
fn installed_with_control() -> (Machine, FakeServer) {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::with_control("1.0.0"));
    machine.install(&server);
    machine.clear_logs();
    (machine, server)
}

fn has_call(machine: &Machine, needle: &str) -> bool {
    machine.calls().iter().any(|call| call.contains(needle))
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// Lance l'agent et rend son code de sortie. Un agent qui attend l'autre bout d'un tube ne rend
/// jamais la main : au bout de 30 s, il est arrêté et l'essai échoue.
fn code_within_30s(machine: &Machine, args: &[&str]) -> Option<i32> {
    let mut child = machine
        .agent()
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("drakbase-stack {args:?} ne rend pas la main");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn install_then_update_twice() {
    let machine = Machine::new();
    let server = FakeServer::start();
    let mut first = Release::new("1.0.0");
    first.pre_update = true;
    machine.publish(&server, &first);

    // Sans terminal ni --accept, les permissions arrêtent l'installation, sans rien laisser.
    let refused = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
    ]);
    refused.assert_code(3);
    refused.assert_contains("app : port 8080/tcp sur toutes les adresses");
    refused.assert_contains("--accept");
    assert!(!machine.stack_dir().exists());
    assert!(machine.applied().is_empty());

    let run = machine.install(&server);
    run.assert_contains("signature vérifiée");
    run.assert_contains("version 1.0.0 en place");
    run.assert_contains("drakbase-stack auto on");
    // Une première installation n'a rien à préparer : pas de pre_update.
    assert_eq!(machine.applied(), ["pull 1.0.0", "up 1.0.0"]);
    assert!(has_call(
        &machine,
        "up -d --remove-orphans --wait --wait-timeout 120"
    ));
    assert!(has_call(&machine, "--profile * config --format json"));
    assert!(has_call(&machine, "compose ls --all --format json"));

    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["previous"], json!(null));
    assert_eq!(state["channel"], "stable");
    assert_eq!(state["trusted_keys"], json!([key("test-app.pub")]));
    assert_eq!(
        state["accepted_permissions"],
        json!(["app|port|*:8080/tcp"])
    );
    let host = server.url.trim_start_matches("http://");
    assert_eq!(state["installed"]["images"], json!([first.image(host)]));
    assert_eq!(machine.env_file(), first.env_example);
    assert_eq!(mode(&machine.stack_dir()), 0o700);
    assert_eq!(mode(&machine.stack_dir().join(".env")), 0o600);
    assert_eq!(
        mode(&machine.release_dir("1.0.0").join("files/app.conf")),
        0o644
    );

    // La vérification : route, paramètres et identifiant d'installation.
    let checks = server.checks();
    let check = checks.last().unwrap();
    assert_eq!(
        check.path,
        format!("/api/v1/update/{APP}/linux/{}/0.0.0", arch())
    );
    assert_eq!(check.query["bundle_type"], "stack");
    assert_eq!(check.query["channel"], "stable");
    assert_eq!(check.query["os_version"], "2.29.1");
    let config = machine.config();
    assert_eq!(config["server"], server.url);
    assert_eq!(check.install_id.as_deref(), config["install_id"].as_str());

    // 1.1.0 : une variable de plus dans .env.example, et deux clés pour la suite.
    fs::write(machine.stack_dir().join(".env"), "# Mon port\nPORT=9000\n").unwrap();
    let mut second = Release::new("1.1.0");
    second.pre_update = true;
    second.env_example = "# Port de l'interface\nPORT=8080\n# Fuseau\nTZ=Europe/Paris\n".into();
    second.public_keys = vec![key("other-app.pub"), key("test-app.pub")];
    machine.publish(&server, &second);
    machine.clear_logs();
    let run = machine.run(&["update", APP]);
    run.assert_code(0);
    run.assert_contains("réglage ajouté à .env : TZ");
    assert_eq!(machine.applied(), ["pull 1.1.0", "run 1.1.0", "up 1.1.0"]);
    assert!(has_call(
        &machine,
        "--profile drakbase-hooks run --rm -T backup"
    ));
    assert_eq!(
        machine.env_file(),
        "# Mon port\nPORT=9000\n\n# Fuseau\nTZ=Europe/Paris\n"
    );
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.1.0");
    assert_eq!(state["previous"]["version"], "1.0.0");
    assert_eq!(
        state["trusted_keys"],
        json!([key("other-app.pub"), key("test-app.pub")])
    );
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
    assert_eq!(
        server.checks().last().unwrap().path,
        format!("/api/v1/update/{APP}/linux/{}/1.0.0", arch())
    );

    // 1.2.0, signée par la clé que 1.1.0 a ajoutée : la 1.0.0 part, avec son image.
    let mut third = Release::new("1.2.0");
    third.signed_by = "other-app.secret";
    machine.publish(&server, &third);
    machine.clear_logs();
    machine.run(&["update", APP]).assert_code(0);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.2.0");
    assert_eq!(state["previous"]["version"], "1.1.0");
    assert_eq!(machine.releases(), ["1.1.0", "1.2.0"]);
    assert!(has_call(
        &machine,
        &format!("image rm {}", first.image(host))
    ));

    // Plus rien de nouveau.
    let run = machine.run(&["update"]);
    run.assert_code(0);
    run.assert_contains("demo : à jour (1.2.0)");
}

#[test]
fn new_permission_stops_until_accept() {
    let (machine, server) = installed();
    let mut next = Release::new("1.1.0");
    next.cap_add = vec!["NET_ADMIN".into()];
    machine.publish(&server, &next);

    let run = machine.run(&["update", APP]);
    run.assert_code(3);
    run.assert_contains("app : capacité NET_ADMIN");
    run.assert_contains("sudo drakbase-stack update demo --accept");
    assert!(machine.applied().is_empty());
    assert_eq!(machine.releases(), ["1.0.0"]);
    assert!(!machine.stack_dir().join(".env.next").exists());
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");

    // Même pour toutes les stacks (le minuteur) : code 3.
    machine.run(&["update"]).assert_code(3);

    let run = machine.run(&["update", APP, "--accept"]);
    run.assert_code(0);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.1.0");
    assert_eq!(
        state["accepted_permissions"],
        json!(["app|capability|NET_ADMIN", "app|port|*:8080/tcp"])
    );
}

#[test]
fn safe_failure_returns_alone_and_rejects() {
    let (machine, server) = installed();
    let mut next = Release::new("1.1.0");
    next.rollback = "safe";
    machine.publish(&server, &next);
    machine.fail("up", "1.1.0");

    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("la version 1.1.0 n'est pas devenue saine");
    run.assert_contains("retour à la version 1.0.0 (rollback : safe)");
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(machine.applied(), ["pull 1.1.0", "up 1.0.0"]);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["failed"], json!(null));
    assert_eq!(state["rejected"], json!(["1.1.0"]));
    assert_eq!(machine.releases(), ["1.0.0"]);

    // Écartée : l'agent ne la reprend pas seul, ni ne la retélécharge.
    let downloads = server.downloads().len();
    let run = machine.run(&["update", APP]);
    run.assert_code(0);
    run.assert_contains("la version 1.1.0 a été écartée");
    assert_eq!(server.downloads().len(), downloads);

    machine.heal("up", "1.1.0");
    machine.run(&["update", APP, "--retry"]).assert_code(0);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.1.0");
    assert_eq!(state["rejected"], json!([]));
}

#[test]
fn manual_failure_stops_then_rollback() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.fail("up", "1.1.0");

    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("Fin du journal");
    run.assert_contains("journal simulé de 1.1.0");
    run.assert_contains("sudo drakbase-stack rollback demo");
    run.assert_contains("sudo drakbase-stack update demo --retry");
    // Les conteneurs restent en l'état : aucun retour seul.
    assert_eq!(machine.applied(), ["pull 1.1.0"]);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["failed"]["version"], "1.1.0");
    assert_eq!(state["rejected"], json!(["1.1.0"]));
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);

    // Le minuteur le rappelle, en échec, sans y toucher.
    let run = machine.run(&["update"]);
    run.assert_code(1);
    run.assert_contains("rollback demo");

    let status = machine.run(&["status"]);
    status.assert_code(0);
    status.assert_contains("en échec : 1.1.0");

    machine.clear_logs();
    let run = machine.run(&["rollback", APP]);
    run.assert_code(0);
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    let state = machine.state();
    assert_eq!(state["failed"], json!(null));
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(machine.releases(), ["1.0.0"]);
}

#[test]
fn restart_applies_the_env_after_checking_permissions() {
    let (machine, server) = installed();
    // Un réglage changé à la main donne, après lecture par Docker Compose, une capacité nouvelle.
    let mut edited = Release::new("1.0.0");
    edited.cap_add = vec!["NET_ADMIN".into()];
    machine.prepare(&server, &edited);

    let run = machine.run(&["restart", APP]);
    run.assert_code(3);
    run.assert_contains("app : capacité NET_ADMIN");
    run.assert_contains("sudo drakbase-stack restart demo --accept");
    assert!(machine.applied().is_empty());

    let run = machine.run(&["restart", APP, "--accept"]);
    run.assert_code(0);
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(
        state["accepted_permissions"],
        json!(["app|capability|NET_ADMIN", "app|port|*:8080/tcp"])
    );

    // Sans permission nouvelle, la relance se fait tout de suite.
    machine.clear_logs();
    machine.run(&["restart", APP]).assert_code(0);
    assert_eq!(machine.applied(), ["up 1.0.0"]);

    machine.run(&["restart", "autre"]).assert_code(1);
}

#[test]
fn rollback_to_previous_rejects_the_left_version() {
    let (machine, server) = installed();
    let next = Release::new("1.1.0");
    machine.publish(&server, &next);
    machine.run(&["update", APP]).assert_code(0);
    machine.clear_logs();

    let run = machine.run(&["rollback", APP]);
    run.assert_code(0);
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["previous"], json!(null));
    assert_eq!(state["rejected"], json!(["1.1.0"]));
    assert_eq!(machine.releases(), ["1.0.0"]);
    let host = server.url.trim_start_matches("http://");
    assert!(has_call(
        &machine,
        &format!("image rm {}", next.image(host))
    ));

    let run = machine.run(&["rollback", APP]);
    run.assert_code(1);
    run.assert_contains("pas de version précédente");
    // Et la version quittée n'est pas reprise seule.
    machine.run(&["update", APP]).assert_contains("écartée");
}

#[test]
fn refuses_a_signature_from_an_unknown_key() {
    let (machine, server) = installed();
    let mut next = Release::new("1.1.0");
    next.signed_by = "other-app.secret";
    machine.publish(&server, &next);

    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("version 1.1.0 refusée");
    run.assert_contains("pas une clé de confiance");
    assert!(!has_call(&machine, "config"));
    assert_eq!(machine.releases(), ["1.0.0"]);
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
    // Une signature refusée n'écarte pas la version : c'est le serveur qui se trompe.
    assert_eq!(machine.state()["rejected"], json!([]));
}

#[test]
fn only_goes_up_with_the_signed_version() {
    let (machine, server) = installed();

    // Le serveur propose la version installée : c'est « à jour ».
    let mut same = Release::new("1.0.0");
    same.always = true;
    server.publish(same.fake_release());
    let run = machine.run(&["update", APP]);
    run.assert_code(0);
    run.assert_contains("à jour (1.0.0)");
    assert!(server.downloads().len() == 1);

    // Une bêta pour le canal stable : « à jour » aussi.
    server.clear();
    let mut beta = Release::new("1.1.0-beta.1");
    beta.beta = true;
    beta.always = true;
    machine.publish(&server, &beta);
    machine
        .run(&["update", APP])
        .assert_contains("à jour (1.0.0)");
    assert!(server.downloads().len() == 1);

    // Annoncée 1.1.0, signée 1.0.9 : refusée.
    server.clear();
    let mut lying = Release::new("1.1.0");
    lying.signed_version = "1.0.9".into();
    machine.publish(&server, &lying);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("version signée « 1.0.9 », attendue « 1.1.0 »");
    assert_eq!(machine.releases(), ["1.0.0"]);
}

#[test]
fn refuses_trapped_archives() {
    let (machine, server) = installed();
    for (name, code) in [
        ("dotdot", "entry_path"),
        ("symlink", "entry_type"),
        ("not-gzip", "archive_invalid"),
    ] {
        server.clear();
        let mut trapped = Release::new("1.1.0");
        trapped.bytes = Some(
            fs::read(
                fixtures()
                    .join("stack/archives")
                    .join(format!("{name}.stack.tar.gz")),
            )
            .unwrap(),
        );
        machine.publish(&server, &trapped);
        let run = machine.run(&["update", APP]);
        run.assert_code(1);
        run.assert_contains(code);
        assert_eq!(machine.releases(), ["1.0.0"], "{name}");
    }
    // Rien n'est sorti du dossier de la stack.
    assert!(!machine.root.path().join("etc/cron.d").exists());
    assert!(!machine.stack_dir().join("etc").exists());

    // stack.json d'une autre version, dans une archive bien signée.
    server.clear();
    let mut other = Release::new("1.1.0");
    other.bytes = Some(fs::read(fixtures().join("stack/demo-1.0.0-aarch64.stack.tar.gz")).unwrap());
    machine.publish(&server, &other);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("version_mismatch");
}

#[test]
fn checks_the_announced_size_and_sha256() {
    let (machine, server) = installed();
    let next = Release::new("1.1.0");
    machine.prepare(&server, &next);

    let mut wrong_sha = next.fake_release();
    wrong_sha.announced_sha256 = Some("0".repeat(64));
    server.publish(wrong_sha);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("SHA-256");

    server.clear();
    let mut wrong_size = next.fake_release();
    wrong_size.announced_size = Some(wrong_size.bytes.len() as u64 + 10);
    server.publish(wrong_size);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("fichier incomplet");

    server.clear();
    let mut short = next.fake_release();
    short.announced_size = Some(10);
    server.publish(short);
    machine
        .run(&["update", APP])
        .assert_contains("dépasse la taille annoncée");
    assert!(!machine.stack_dir().join("releases/.partial").exists());
    assert_eq!(machine.releases(), ["1.0.0"]);

    // Une archive annoncée au-delà de la limite d'une stack n'est pas téléchargée ; sa taille
    // est dite en unités binaires, celles de la limite.
    server.clear();
    let mut huge = next.fake_release();
    huge.announced_size = Some(64 * 1024 * 1024);
    server.publish(huge);
    let downloads = server.downloads().len();
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("annonce une archive de 64,0 Mio, bien plus qu'une stack");
    assert_eq!(server.downloads().len(), downloads);
}

#[test]
fn checks_docker_before_anything() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::new("1.0.0"));
    let install = || {
        machine.run(&[
            "install",
            APP,
            "--key",
            &key("test-app.pub"),
            "--server",
            &server.url,
            "--accept",
        ])
    };

    machine.set_engine("23.0.6");
    let run = install();
    run.assert_code(1);
    run.assert_contains("Docker Engine 23.0.6 est trop ancien");

    machine.set_engine("24.0.9");
    machine.set_compose("v2.28.1");
    let run = install();
    run.assert_code(1);
    run.assert_contains("Docker Compose 2.28.1 est trop ancien");

    machine.daemon_down(true);
    let run = install();
    run.assert_code(1);
    run.assert_contains("le service Docker ne répond pas");
    machine.daemon_down(false);

    let mut missing = machine.agent();
    missing.env("DRAKBASE_STACK_DOCKER", machine.root.path().join("absent"));
    let output = missing.args(["update"]).output().unwrap();
    // Sans stack, update ne touche pas à Docker.
    assert_eq!(output.status.code(), Some(0));
    assert!(server.checks().is_empty());
    assert!(!machine.stack_dir().exists());

    // Docker 24 et Compose 2.29, avec le « v » : c'est bon.
    machine.set_compose("v2.29.1");
    install().assert_code(0);
    assert_eq!(server.checks()[0].query["os_version"], "2.29.1");
}

#[test]
fn install_refusals_and_usage() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::new("1.0.0"));

    // Un projet Compose du même nom, pas créé par l'agent.
    machine.set_projects(
        r#"[{"Name":"demo","Status":"running(1)","ConfigFiles":"/opt/demo/compose.yaml"}]"#,
    );
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
        "--accept",
    ]);
    run.assert_code(1);
    run.assert_contains("existe déjà sur cette machine");
    assert!(server.checks().is_empty());
    machine.set_projects("[]");

    // Clé illisible, app mal écrite : usage.
    machine
        .run(&[
            "install",
            APP,
            "--key",
            "pas une clé",
            "--server",
            &server.url,
        ])
        .assert_code(2);
    machine
        .run(&["install", "Demo", "--key", &key("test-app.pub")])
        .assert_code(2);
    machine.run(&["update", "--inconnue"]).assert_code(2);
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--channel",
        "nightly",
    ]);
    run.assert_code(2);
    run.assert_contains("stable ou beta");

    // Rien de publié pour cette app sur ce canal.
    let empty = FakeServer::start();
    empty.declare(APP);
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &empty.url,
        "--accept",
    ]);
    run.assert_code(1);
    run.assert_contains("aucune version de demo n'est publiée");
    assert!(!machine.stack_dir().exists());

    machine.install(&server);
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
    ]);
    run.assert_code(1);
    run.assert_contains("déjà installée");

    let run = machine.run(&["update", "autre"]);
    run.assert_code(1);
    run.assert_contains("autre n'est pas installée");
}

#[test]
fn beta_channel_and_no_stats() {
    let machine = Machine::new();
    let server = FakeServer::start();
    let mut beta = Release::new("1.1.0-beta.1");
    beta.beta = true;
    machine.publish(&server, &Release::new("1.0.0"));
    machine.publish(&server, &beta);

    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
        "--channel",
        "beta",
        "--no-stats",
        "--accept",
    ]);
    run.assert_code(0);
    let check = server.checks().last().unwrap().clone();
    assert_eq!(check.query["channel"], "beta");
    assert_eq!(check.install_id, None);
    assert_eq!(machine.config()["stats"], false);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.1.0-beta.1");
    assert_eq!(state["channel"], "beta");
}

#[test]
fn check_examines_without_installing() {
    let (machine, server) = installed_with_control();
    // Après l'installation, l'app lit déjà son état.
    let status = machine.status();
    assert_eq!(status["installed"], "1.0.0");
    assert_eq!(status["available"], json!(null));

    let mut next = Release::with_control("1.1.0");
    // Fins de ligne Windows, et une séquence qui effacerait la ligne au terminal.
    next.notes = Some("- Nouvelle page\r\n- Correctifs\u{1b}[2K".into());
    machine.publish(&server, &next);
    let downloads = server.downloads().len();

    let run = machine.run(&["check", APP]);
    run.assert_code(0);
    run.assert_contains("version 1.1.0 disponible (stable), installée : 1.0.0");
    run.assert_contains("  - Nouvelle page\n  - Correctifs\\u{1b}[2K\n");
    run.assert_contains("signature vérifiée");
    run.assert_contains(
        "Pour l'installer : depuis l'interface de l'app, ou sudo drakbase-stack update demo",
    );
    // L'archive est téléchargée et vérifiée, mais rien ne change de ce qui tourne.
    assert_eq!(server.downloads().len(), downloads + 1);
    assert!(machine.applied().is_empty());
    assert_eq!(machine.releases(), ["1.0.0"]);
    assert!(!machine.stack_dir().join(".check").exists());
    assert!(!machine.stack_dir().join(".env.check").exists());
    let status = machine.status();
    assert_eq!(status["installed"], "1.0.0");
    assert_eq!(status["available"]["version"], "1.1.0");
    assert_eq!(
        status["available"]["notes"],
        "- Nouvelle page\n- Correctifs\\u{1b}[2K"
    );
    assert_eq!(status["available"]["new_permissions"], json!([]));
    assert!(status["checked_at"].is_string());

    // Une version qui demande une permission nouvelle le dit, sans proposer de l'installer.
    let mut wider = Release::with_control("1.2.0");
    wider.cap_add = vec!["NET_ADMIN".into()];
    machine.publish(&server, &wider);
    let run = machine.run(&["check"]);
    run.assert_code(0);
    run.assert_contains("sudo drakbase-stack update demo --accept");
    assert_eq!(
        machine.status()["available"]["new_permissions"],
        json!(["app : capacité NET_ADMIN"])
    );

    // Une fois installée, la version proposée est oubliée.
    machine.run(&["update", APP, "--accept"]).assert_code(0);
    assert_eq!(machine.status()["available"], json!(null));
    assert_eq!(machine.status()["installed"], "1.2.0");
}

#[test]
fn the_interface_asks_and_the_agent_installs() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::with_control("1.0.0"));
    machine.install(&server);
    // Le fichier Compose de l'agent monte le dossier de contrôle dans le service.
    let control = machine.stack_dir().join("control");
    let compose: Value = serde_json::from_slice(
        &fs::read(
            machine
                .stack_dir()
                .join("releases/1.0.0/drakbase-control.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        compose["services"]["app"]["environment"]["DRAKBASE_CONTROL"],
        "/run/drakbase"
    );
    // Le dossier en lecture seule, et `request/` par-dessus, en écriture : l'app n'écrit que là.
    assert_eq!(
        compose["services"]["app"]["volumes"],
        json!([
            {
                "type": "bind",
                "source": control.to_string_lossy(),
                "target": "/run/drakbase",
                "read_only": true
            },
            {
                "type": "bind",
                "source": control.join("request").to_string_lossy(),
                "target": "/run/drakbase/request"
            }
        ])
    );
    // L'app écrit dans `request/` sous l'utilisateur de son conteneur ; les autres comptes de
    // la machine n'entrent pas dans le dossier de la stack.
    assert_eq!(mode(&control.join("request")), 0o777);
    assert_eq!(mode(&control), 0o755);
    assert_eq!(mode(&machine.stack_dir()), 0o700);
    // Il sert à up, pas au contrôle des permissions (config).
    let calls = machine.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.contains(" up ") && call.contains("drakbase-control.json")),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .all(|call| !(call.contains(" config ") && call.contains("drakbase-control.json"))),
        "{calls:#?}"
    );
    machine.clear_logs();

    machine.publish(&server, &Release::with_control("1.1.0"));

    // Une demande pour une version que la vérification n'a pas proposée est refusée.
    machine.request(
        "1-update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    let run = machine.run(&["request", APP]);
    run.assert_code(1);
    let status = machine.status();
    assert_eq!(status["operation"]["state"], "failed");
    assert!(
        status["operation"]["message"]
            .as_str()
            .unwrap()
            .contains("vérifiez de nouveau")
    );
    assert!(machine.applied().is_empty());
    assert_eq!(fs::read_dir(control.join("request")).unwrap().count(), 0);

    // L'interface demande une vérification, puis l'installation de la version proposée.
    machine.request("2-check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(0);
    let status = machine.status();
    assert_eq!(status["operation"]["action"], "check");
    assert_eq!(status["operation"]["state"], "done");
    assert_eq!(status["operation"]["message"], "version 1.1.0 disponible");
    assert_eq!(status["available"]["version"], "1.1.0");

    machine.request(
        "3-update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    let run = machine.run(&["request", APP]);
    run.assert_code(0);
    assert_eq!(machine.applied(), ["pull 1.1.0", "up 1.1.0"]);
    let status = machine.status();
    assert_eq!(status["installed"], "1.1.0");
    assert_eq!(status["available"], json!(null));
    assert_eq!(status["operation"]["action"], "update");
    assert_eq!(status["operation"]["version"], "1.1.0");
    assert_eq!(status["operation"]["state"], "done");
    assert_eq!(status["operation"]["message"], "version 1.1.0 en place");
    assert!(status["operation"]["finished_at"].is_string());

    // Une demande illisible est retirée, et dite refusée.
    machine.request("4-x.json", "installe tout");
    machine.run(&["request", APP]).assert_code(1);
    let status = machine.status();
    assert_eq!(status["operation"]["state"], "failed");
    assert!(
        status["operation"]["message"]
            .as_str()
            .unwrap()
            .starts_with("demande refusée")
    );
    assert_eq!(fs::read_dir(control.join("request")).unwrap().count(), 0);
    // Une version qui n'est pas canonique : le refus parle de la demande, pas de stack.json.
    machine.request(
        "5-v.json",
        r#"{"format": 1, "action": "update", "version": "v1.2"}"#,
    );
    machine.run(&["request", APP]).assert_code(1);
    assert_eq!(
        machine.status()["operation"]["message"],
        "demande refusée : demande d'une version « v1.2 » non canonique"
    );
    // Sans demande, rien à faire.
    machine.run(&["request", APP]).assert_code(0);
}

#[test]
fn a_request_installs_only_the_version_the_user_validated() {
    let (machine, server) = installed_with_control();
    machine.publish(&server, &Release::with_control("1.1.0"));
    machine.run(&["check", APP]).assert_code(0);
    assert_eq!(machine.status()["available"]["version"], "1.1.0");

    // Une 1.2.0 est publiée après la vérification : l'utilisateur n'a lu que les notes de la
    // 1.1.0, et c'est elle qu'il valide.
    machine.publish(&server, &Release::with_control("1.2.0"));
    machine.clear_logs();
    let downloads = server.downloads().len();
    let ask = |version: &str| {
        machine.request(
            "update.json",
            &format!(r#"{{"format": 1, "action": "update", "version": "{version}"}}"#),
        );
        machine.run(&["request", APP])
    };
    ask("1.1.0").assert_code(1);
    // Rien n'est téléchargé ni installé, et l'app apprend pourquoi.
    assert_eq!(server.downloads().len(), downloads);
    assert!(machine.applied().is_empty());
    assert_eq!(machine.releases(), ["1.0.0"]);
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
    let status = machine.status();
    assert_eq!(status["installed"], "1.0.0");
    assert_eq!(status["operation"]["action"], "update");
    assert_eq!(status["operation"]["version"], "1.1.0");
    assert_eq!(status["operation"]["state"], "failed");
    let message = status["operation"]["message"].as_str().unwrap();
    assert!(
        message.contains("la version 1.2.0") && message.contains("vérifiez de nouveau"),
        "{message}"
    );

    // Le serveur ne propose plus rien : même refus, sans rien installer.
    server.clear();
    ask("1.1.0").assert_code(1);
    assert_eq!(server.downloads().len(), downloads);
    assert!(machine.applied().is_empty());
    let status = machine.status();
    assert_eq!(status["installed"], "1.0.0");
    assert_eq!(status["operation"]["state"], "failed");
    let message = status["operation"]["message"].as_str().unwrap();
    assert!(
        message.contains("n'est plus proposée") && message.contains("vérifiez de nouveau"),
        "{message}"
    );

    // Après une nouvelle vérification, la 1.2.0 est proposée, puis installée à la demande.
    machine.publish(&server, &Release::with_control("1.2.0"));
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(machine.status()["available"]["version"], "1.2.0");
    ask("1.2.0").assert_code(0);
    assert_eq!(machine.applied(), ["pull 1.2.0", "up 1.2.0"]);
    assert_eq!(machine.status()["installed"], "1.2.0");
}

/// Une app en root dans son conteneur a pu remplacer `request` par un lien, tant que `control/`
/// était monté en écriture : l'agent ne touche jamais à la cible.
#[test]
fn a_link_in_place_of_the_request_folder_is_never_followed() {
    let (machine, server) = installed_with_control();
    let requests = machine.stack_dir().join("control/request");
    // Un dossier de la machine, hors de la stack.
    let outside = machine.root.path().join("ailleurs");
    fs::create_dir_all(outside.join("sous-dossier")).unwrap();
    fs::write(outside.join("precieux.txt"), "à garder").unwrap();
    fs::write(
        outside.join("sous-dossier/demande.json"),
        r#"{"format": 1, "action": "check"}"#,
    )
    .unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
    let untouched = || {
        assert_eq!(
            fs::read_to_string(outside.join("precieux.txt")).unwrap(),
            "à garder"
        );
        assert!(outside.join("sous-dossier/demande.json").exists());
        assert_eq!(mode(&outside), 0o750);
    };
    let checks = server.checks().len();

    fs::remove_dir_all(&requests).unwrap();
    symlink(&outside, &requests).unwrap();
    // `check` écrit `status.json` à côté, sans passer par le lien : sa cible n'est ni vidée, ni
    // ouverte à tous.
    machine.run(&["check", APP]).assert_code(0);
    untouched();
    assert!(machine.status()["checked_at"].is_string());
    // `request` remet un vrai dossier, vide, sans rien lire dans le dossier lié.
    machine.run(&["request", APP]).assert_code(0);
    untouched();
    let metadata = fs::symlink_metadata(&requests).unwrap();
    assert!(metadata.file_type().is_dir());
    assert_eq!(mode(&requests), 0o777);
    assert_eq!(fs::read_dir(&requests).unwrap().count(), 0);
    // Seul `check` a interrogé le serveur.
    assert_eq!(server.checks().len(), checks + 1);

    // Une relance le remet aussi, avant de monter le dossier dans le service.
    fs::remove_dir_all(&requests).unwrap();
    symlink(&outside, &requests).unwrap();
    machine.run(&["restart", APP]).assert_code(0);
    untouched();
    assert!(fs::symlink_metadata(&requests).unwrap().is_dir());
    assert_eq!(mode(&requests), 0o777);

    // Un fichier à la place du dossier : retiré, et le dossier recréé.
    fs::remove_dir_all(&requests).unwrap();
    fs::write(&requests, "pas un dossier").unwrap();
    machine.run(&["request", APP]).assert_code(0);
    assert!(fs::symlink_metadata(&requests).unwrap().is_dir());
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(machine.status()["operation"]["state"], "done");
}

/// Dans `request/`, où l'app écrit, seul un fichier ordinaire de 4 Kio au plus est une demande.
#[test]
fn only_small_regular_files_are_read_as_requests() {
    let (machine, server) = installed_with_control();
    let requests = machine.stack_dir().join("control/request");
    let outside = machine.root.path().join("ailleurs");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precieux.txt"), "à garder").unwrap();
    // Un fichier de la machine qui se lirait comme une demande valide.
    let victim = machine.root.path().join("victime.json");
    fs::write(&victim, r#"{"format": 1, "action": "check"}"#).unwrap();

    symlink(&victim, requests.join("1-lien.json")).unwrap();
    symlink(&outside, requests.join("2-dossier-lie.json")).unwrap();
    fs::create_dir_all(requests.join("3-dossier.json/profond")).unwrap();
    fs::write(requests.join("3-dossier.json/profond/x"), "x").unwrap();
    symlink(&outside, requests.join("3-dossier.json/lien")).unwrap();
    let fifo = Command::new("mkfifo")
        .arg(requests.join("4-tube.json"))
        .status()
        .unwrap();
    assert!(fifo.success());
    fs::write(requests.join("5-grosse.json"), " ".repeat(5000)).unwrap();
    let checks = server.checks().len();

    // Cinq demandes refusées, sans attendre l'autre bout du tube.
    assert_eq!(code_within_30s(&machine, &["request", APP]), Some(1));
    assert_eq!(fs::read_dir(&requests).unwrap().count(), 0);
    // Le fichier lié n'a pas été lu comme une demande, et les cibles des liens sont intactes.
    assert_eq!(server.checks().len(), checks);
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        r#"{"format": 1, "action": "check"}"#
    );
    assert_eq!(
        fs::read_to_string(outside.join("precieux.txt")).unwrap(),
        "à garder"
    );
    let status = machine.status();
    assert_eq!(status["operation"]["state"], "failed");
    assert_eq!(
        status["operation"]["message"],
        "demande refusée : demande de plus de 4 Kio"
    );

    // Chaque refus dit pourquoi.
    symlink(&victim, requests.join("lien.json")).unwrap();
    let run = machine.run(&["request", APP]);
    run.assert_code(1);
    run.assert_contains("« lien.json » n'est pas un fichier ordinaire");

    // Une vraie demande passe toujours.
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(server.checks().len(), checks + 1);
    assert_eq!(machine.status()["operation"]["state"], "done");
}

/// `status.json` est écrit sans rien ouvrir dans `control/` : un lien posé à la place du fichier,
/// ou de son fichier temporaire, n'est pas suivi.
#[test]
fn status_is_never_written_through_a_link() {
    let (machine, _server) = installed_with_control();
    let control = machine.stack_dir().join("control");
    let status = control.join("status.json");
    let victim = machine.root.path().join("victime.txt");
    fs::write(&victim, "intact").unwrap();

    symlink(&victim, control.join(".status.json.tmp")).unwrap();
    fs::remove_file(&status).unwrap();
    symlink(&victim, &status).unwrap();
    machine.run(&["check", APP]).assert_code(0);
    assert_eq!(fs::read_to_string(&victim).unwrap(), "intact");
    assert!(fs::symlink_metadata(&status).unwrap().is_file());
    assert_eq!(machine.status()["installed"], "1.0.0");

    // Un dossier à la place du fichier.
    fs::remove_file(&status).unwrap();
    fs::create_dir_all(status.join("sous-dossier")).unwrap();
    machine.run(&["check", APP]).assert_code(0);
    assert!(fs::symlink_metadata(&status).unwrap().is_file());
    assert_eq!(machine.status()["installed"], "1.0.0");
    assert!(!machine.stack_dir().join(".status.json.tmp").exists());
}

/// Toute commande qui prend le verrou de la stack requalifie une opération restée « running » :
/// celles de l'interface, comme celles tapées sur la machine.
#[test]
fn an_operation_left_running_is_marked_interrupted() {
    let (machine, _server) = installed_with_control();
    // `rollback` échoue ici, faute de version précédente : l'opération est requalifiée quand même.
    let commands = [
        ("request", 0),
        ("check", 0),
        ("update", 0),
        ("restart", 0),
        ("rollback", 1),
    ];
    for (command, code) in commands {
        // Un processus arrêté au milieu d'une mise à jour a laissé l'opération « running », dans
        // son état comme dans ce que lit l'app.
        for file in ["state.json", "control/status.json"] {
            let path = machine.stack_dir().join(file);
            let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            document["operation"] = json!({
                "action": "update", "version": "1.1.0", "state": "running",
                "started_at": "2026-11-13T08:30:02Z", "finished_at": null,
                "message": "installation de la version 1.1.0"
            });
            fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        }

        machine.run(&[command, APP]).assert_code(code);
        for document in [machine.state(), machine.status()] {
            let operation = &document["operation"];
            assert_eq!(operation["state"], "failed", "{command}");
            assert_eq!(
                operation["message"], "opération interrompue (installation de la version 1.1.0)",
                "{command}"
            );
            assert!(operation["finished_at"].is_string(), "{command}");
        }
    }
}

/// Une installation qui s'arrête avant le démarrage ne laisse rien de la stack, même quand son
/// dossier de contrôle a déjà été préparé pour le montage.
#[test]
fn an_install_that_stops_before_starting_leaves_nothing() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::with_control("1.0.0"));
    let install = || {
        machine.run(&[
            "install",
            APP,
            "--key",
            &key("test-app.pub"),
            "--server",
            &server.url,
            "--accept",
        ])
    };

    machine.fail("pull", "1.0.0");
    let run = install();
    run.assert_code(1);
    run.assert_contains("rien n'a été démarré");
    assert!(!machine.stack_dir().exists());

    // Après une désinstallation, seul le .env gardé reste (et le verrou de la commande).
    machine.heal("pull", "1.0.0");
    install().assert_code(0);
    machine.run(&["uninstall", APP]).assert_code(0);
    machine.fail("pull", "1.0.0");
    install().assert_code(1);
    let mut left: Vec<String> = fs::read_dir(machine.stack_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [".env", ".lock"]);
}

#[test]
fn new_permissions_are_never_accepted_from_the_interface() {
    let (machine, server) = installed_with_control();
    let mut next = Release::with_control("1.1.0");
    next.cap_add = vec!["NET_ADMIN".into()];
    machine.publish(&server, &next);
    machine.run(&["check", APP]).assert_code(0);
    machine.clear_logs();

    machine.request(
        "update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    machine.run(&["request", APP]).assert_code(3);
    let status = machine.status();
    assert_eq!(status["operation"]["state"], "permissions");
    assert!(
        status["operation"]["message"]
            .as_str()
            .unwrap()
            .contains("sudo drakbase-stack update demo --accept")
    );
    assert!(machine.applied().is_empty());
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
}

/// Sans `app_control`, une stack n'a ni dossier de contrôle, ni unité des demandes, et `request`
/// la refuse : aucun compte de la machine ne peut y déposer de demande.
#[test]
fn a_stack_without_app_control_has_no_control_folder() {
    let (machine, server) = installed();
    let control = machine.stack_dir().join("control");
    assert!(!control.exists());
    assert_eq!(mode(&machine.stack_dir()), 0o700);

    // Ni la vérification, ni la relance ne le créent.
    machine.publish(&server, &Release::new("1.1.0"));
    machine.run(&["check", APP]).assert_code(0);
    machine.run(&["restart", APP]).assert_code(0);
    assert!(!control.exists());

    let checks = server.checks().len();
    let run = machine.run(&["request", APP]);
    run.assert_code(1);
    run.assert_contains("demo ne reçoit pas de demande de son interface");
    run.assert_contains("sudo drakbase-stack update demo");
    assert!(!control.exists());
    assert_eq!(server.checks().len(), checks);

    // `auto on` n'active pas son unité des demandes.
    machine.clear_logs();
    machine.run(&["auto", "on"]).assert_code(0);
    assert_eq!(
        machine.systemctl_calls(),
        [
            "daemon-reload",
            "enable --now drakbase-stack.timer",
            "disable --now drakbase-stack-request@demo.path"
        ]
    );
    // Une mise à jour non plus.
    machine.clear_logs();
    machine.run(&["update", APP]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.1.0");
    assert!(!control.exists());
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );
}

/// Un agent plus ancien créait `control/request/` en 0777 pour toute stack, dans un dossier en
/// 0755, et lui donnait une unité des demandes : la commande suivante referme le dossier de la
/// stack, `request` ne lit pas ce qu'un compte de la machine y a déposé et désactive l'unité, et
/// `auto on` retire le dossier.
#[test]
fn a_control_folder_left_by_an_older_agent_is_closed_then_removed() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.run(&["auto", "on"]).assert_code(0);
    let control = machine.stack_dir().join("control");
    let requests = control.join("request");
    fs::create_dir_all(&requests).unwrap();
    fs::set_permissions(&requests, fs::Permissions::from_mode(0o777)).unwrap();
    let dropped = requests.join("update.json");

    // Chaque commande qui prend le verrou de la stack referme son dossier, et retire ce qui
    // attendait dans `request/` tant qu'il était ouvert.
    for command in ["check", "request"] {
        fs::write(&dropped, r#"{"format": 1, "action": "check"}"#).unwrap();
        fs::set_permissions(machine.stack_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        machine.run(&[command, APP]);
        assert_eq!(mode(&machine.stack_dir()), 0o700, "{command}");
        assert!(!dropped.exists(), "{command}");
    }

    // Une demande restée dans le dossier fermé n'est ni lue, ni suivie d'effet ; l'unité, qui
    // relancerait `request` tant que le dossier n'est pas vide, est désactivée.
    fs::write(&dropped, r#"{"format": 1, "action": "check"}"#).unwrap();
    machine.clear_logs();
    let checks = server.checks().len();
    machine.run(&["request", APP]).assert_code(1);
    assert!(dropped.exists());
    assert_eq!(server.checks().len(), checks);
    assert!(machine.applied().is_empty());
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );

    // `auto on`, une relance et une mise à jour retirent le dossier.
    for command in [&["auto", "on"][..], &["restart", APP], &["update", APP]] {
        fs::create_dir_all(&requests).unwrap();
        machine.run(command).assert_code(0);
        assert!(!control.exists(), "{command:?}");
    }
}

/// Le dossier de contrôle et l'unité des demandes suivent la version qui tourne : une version
/// qui retire `app_control` les retire, un retour à une version qui l'a les remet.
#[test]
fn the_control_folder_and_the_request_unit_follow_the_running_version() {
    let (machine, server) = installed_with_control();
    let control = machine.stack_dir().join("control");
    assert_eq!(machine.status()["installed"], "1.0.0");
    // Les unités de l'agent ne sont pas installées : rien n'est demandé à systemd.
    assert!(machine.systemctl_calls().is_empty());

    machine.run(&["auto", "on"]).assert_code(0);
    assert_eq!(
        machine.systemctl_calls(),
        [
            "daemon-reload",
            "enable --now drakbase-stack.timer",
            "enable --now drakbase-stack-request@demo.path"
        ]
    );
    assert_eq!(machine.status()["installed"], "1.0.0");

    // La 1.1.0 n'a plus `app_control`.
    machine.publish(&server, &Release::new("1.1.0"));
    machine.clear_logs();
    machine.run(&["update", APP]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.1.0");
    assert!(!control.exists());
    assert!(
        !machine
            .release_dir("1.1.0")
            .join("drakbase-control.json")
            .exists()
    );
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );
    machine.run(&["request", APP]).assert_code(1);
    assert!(!control.exists());

    // Retour à la 1.0.0, qui l'a.
    machine.clear_logs();
    machine.run(&["rollback", APP]).assert_code(0);
    assert_eq!(mode(&control.join("request")), 0o777);
    assert_eq!(machine.status()["installed"], "1.0.0");
    assert_eq!(
        machine.systemctl_calls(),
        ["enable --now drakbase-stack-request@demo.path"]
    );
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(machine.status()["operation"]["state"], "done");
}

/// `rollback` vers une version sans `app_control` retire le dossier de contrôle, depuis une
/// version saine comme depuis une version restée en échec.
#[test]
fn rollback_to_a_version_without_app_control_drops_the_control_folder() {
    let (machine, server) = installed();
    let control = machine.stack_dir().join("control");
    machine.run(&["auto", "on"]).assert_code(0);

    // Depuis une version saine qui l'a.
    machine.publish(&server, &Release::with_control("1.1.0"));
    machine.clear_logs();
    machine.run(&["update", APP]).assert_code(0);
    assert_eq!(machine.status()["installed"], "1.1.0");
    assert_eq!(
        machine.systemctl_calls(),
        ["enable --now drakbase-stack-request@demo.path"]
    );
    machine.clear_logs();
    machine.run(&["rollback", APP]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
    assert!(!control.exists());
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );

    // Depuis une version restée en échec : ses conteneurs gardent le dossier jusqu'au retour.
    machine.publish(&server, &Release::with_control("1.2.0"));
    machine.fail("up", "1.2.0");
    machine.clear_logs();
    machine.run(&["update", APP]).assert_code(1);
    assert_eq!(machine.state()["failed"]["version"], "1.2.0");
    assert_eq!(machine.status()["failed"], "1.2.0");
    assert!(machine.systemctl_calls().is_empty());
    machine.run(&["rollback", APP]).assert_code(0);
    assert_eq!(machine.state()["failed"], json!(null));
    assert!(!control.exists());
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );
}

/// Une première installation qui échoue au démarrage n'a pas d'unité des demandes ; la reprise
/// qui aboutit la lui donne.
#[test]
fn a_retried_first_install_gets_its_request_unit() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.run(&["auto", "on"]).assert_code(0);
    machine.publish(&server, &Release::with_control("1.0.0"));
    machine.fail("up", "1.0.0");
    machine.clear_logs();
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
        "--accept",
    ]);
    run.assert_code(1);
    assert_eq!(machine.state()["failed"]["version"], "1.0.0");
    assert!(machine.systemctl_calls().is_empty());

    machine.heal("up", "1.0.0");
    machine.run(&["update", APP, "--retry"]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
    assert_eq!(
        machine.systemctl_calls(),
        ["enable --now drakbase-stack-request@demo.path"]
    );
}

/// La reprise d'une version restée en échec, dont les conteneurs sont en l'état, ne lui retire
/// pas son dossier quand elle s'arrête avant le démarrage.
#[test]
fn a_retry_that_stops_early_keeps_the_failed_version() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.fail("up", "1.1.0");
    machine.run(&["update", APP]).assert_code(1);
    let failed = machine.state()["failed"].clone();
    assert_eq!(failed["version"], "1.1.0");
    let marker = machine.release_dir("1.1.0").join("files/marqueur");
    fs::write(&marker, "en place").unwrap();

    // Arrêt au contrôle du compose : le dossier de la version n'a pas été touché.
    machine.fail("config", "1.1.0");
    machine.run(&["update", APP, "--retry"]).assert_code(1);
    assert!(marker.exists());
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
    assert_eq!(machine.state()["failed"], failed);
    machine.heal("config", "1.1.0");

    // Arrêt sur une permission nouvelle, que la version examinée à côté demande : pas touché
    // non plus, et rien ne reste de l'examen.
    let mut wider = Release::new("1.1.0");
    wider.cap_add = vec!["NET_ADMIN".into()];
    machine.prepare(&server, &wider);
    let run = machine.run(&["update", APP, "--retry"]);
    run.assert_code(3);
    run.assert_contains("app : capacité NET_ADMIN");
    assert!(marker.exists());
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
    assert_eq!(machine.state()["failed"], failed);
    assert!(!machine.stack_dir().join(".env.next").exists());
    machine.prepare(&server, &Release::new("1.1.0"));

    // Arrêt au téléchargement des images : la version examinée a pris sa place, entière.
    machine.fail("pull", "1.1.0");
    let run = machine.run(&["update", APP, "--retry"]);
    run.assert_code(1);
    run.assert_contains("images de la version 1.1.0 indisponibles");
    // Ce sont les conteneurs de la version en échec qui sont en place, pas la 1.0.0.
    run.assert_contains("les conteneurs de la version 1.1.0 restent en l'état");
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
    for file in ["stack.json", "compose.yaml", "files/app.conf"] {
        assert!(machine.release_dir("1.1.0").join(file).is_file(), "{file}");
    }
    assert!(!marker.exists());
    assert_eq!(machine.state()["failed"], failed);
    assert!(!machine.stack_dir().join(".env.next").exists());
    machine.run(&["status"]).assert_contains("en échec : 1.1.0");

    // Le retour à la dernière version saine reste possible.
    machine.clear_logs();
    let run = machine.run(&["rollback", APP]);
    run.assert_code(0);
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    assert_eq!(machine.state()["failed"], json!(null));
    assert_eq!(machine.releases(), ["1.0.0"]);

    // Et la reprise, une fois les images disponibles, aboutit.
    machine.heal("pull", "1.1.0");
    machine.heal("up", "1.1.0");
    machine.run(&["update", APP, "--retry"]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.1.0");
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
}

/// Un fichier en cours d'écriture (nom en `.…`) dont la date est dans le futur ne serait jamais
/// périmé : il est retiré tout de suite, sans attente, et le dossier des demandes redevient vide.
#[test]
fn a_request_being_written_with_a_future_date_is_dropped() {
    let (machine, _server) = installed_with_control();
    let requests = machine.stack_dir().join("control/request");
    let temp = requests.join(".en-cours");
    fs::write(&temp, "{").unwrap();
    fs::File::options()
        .write(true)
        .open(&temp)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(3600))
        .unwrap();

    let started = Instant::now();
    machine.run(&["request", APP]).assert_code(0);
    assert!(!temp.exists());
    assert_eq!(fs::read_dir(&requests).unwrap().count(), 0);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "request a attendu {:?}",
        started.elapsed()
    );
}

/// L'interface apprend une seule fois ce qui continue de tourner.
#[test]
fn a_request_that_stops_at_pull_says_once_what_keeps_running() {
    let (machine, server) = installed_with_control();
    machine.publish(&server, &Release::with_control("1.1.0"));
    machine.run(&["check", APP]).assert_code(0);
    machine.fail("pull", "1.1.0");
    machine.request(
        "update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    machine.run(&["request", APP]).assert_code(1);
    let status = machine.status();
    assert_eq!(status["operation"]["state"], "failed");
    let message = status["operation"]["message"].as_str().unwrap();
    assert_eq!(
        message
            .matches("la version 1.0.0 continue de tourner")
            .count(),
        1,
        "{message}"
    );
}

/// Une version qui n'aboutit pas ne laisse pas ses réglages à la version gardée : la 1.1.0
/// ajoute à `.env` un réglage qui donnerait une capacité de plus à la 1.0.0, n'a elle-même aucune
/// permission nouvelle, et ne devient pas saine. La 1.0.0 repart avec le `.env` d'avant.
#[test]
fn a_version_that_does_not_start_leaves_the_settings_as_they_were() {
    let (machine, server) = installed();
    let mut widened = Release::new("1.0.0");
    widened.cap_add = vec!["NET_ADMIN".into()];
    machine.prepare_when(&server, "MODE=wide", &widened);
    let before = machine.env_file();
    let accepted = json!(["app|port|*:8080/tcp"]);

    let mut next = Release::new("1.1.0");
    next.rollback = "safe";
    next.env_example = "# Port de l'interface\nPORT=8080\n# Mode\nMODE=wide\n".into();
    machine.publish(&server, &next);

    // Arrêt au téléchargement des images, après l'étape 6.
    machine.fail("pull", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("réglage ajouté à .env : MODE");
    run.assert_contains(".env remis comme avant ; réglages retirés : MODE");
    assert_eq!(machine.env_file(), before);
    machine.heal("pull", "1.1.0");

    // Échec au démarrage, retour seul : la 1.0.0 est contrôlée, puis relancée, sans permission
    // nouvelle.
    machine.fail("up", "1.1.0");
    machine.clear_logs();
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(machine.env_file(), before);
    assert_eq!(machine.applied(), ["pull 1.1.0", "up 1.0.0"]);
    let calls = machine.calls();
    let checked = calls
        .iter()
        .position(|call| call.contains("releases/1.0.0 ") && call.contains(" config "));
    let relaunched = calls
        .iter()
        .position(|call| call.contains("releases/1.0.0 ") && call.contains(" up "));
    assert!(checked.is_some() && checked < relaunched, "{calls:#?}");
    assert_eq!(machine.state()["accepted_permissions"], accepted);

    // Échec au démarrage laissé en l'état : le .env d'avant revient aussi, pour `rollback`.
    let mut manual = next.clone();
    manual.version = "1.2.0".into();
    manual.signed_version = "1.2.0".into();
    manual.rollback = "manual";
    machine.publish(&server, &manual);
    machine.fail("up", "1.2.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains(
        "Ses réglages nouveaux (MODE) reviendront alors avec leur valeur par défaut",
    );
    assert_eq!(machine.state()["failed"]["version"], "1.2.0");
    assert_eq!(machine.env_file(), before);
    machine.clear_logs();
    machine.run(&["rollback", APP]).assert_code(0);
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    assert_eq!(machine.state()["accepted_permissions"], accepted);

    // Seul le succès garde le .env complété.
    machine.heal("up", "1.2.0");
    machine.run(&["update", APP, "--retry"]).assert_code(0);
    assert_eq!(
        machine.env_file(),
        "# Port de l'interface\nPORT=8080\n\n# Mode\nMODE=wide\n"
    );
}

/// Un `.env` changé à la main pendant la mise à jour garde ces changements quand la version
/// échoue : seuls les réglages qu'elle y avait ajoutés en sont retirés.
#[test]
fn a_settings_file_changed_during_the_update_keeps_its_changes() {
    let (machine, server) = installed();
    let before = machine.env_file();
    let mut next = Release::new("1.1.0");
    next.rollback = "safe";
    next.env_example = "# Port de l'interface\nPORT=8080\n# Mode\nMODE=wide\n".into();
    machine.publish(&server, &next);
    machine.edit_env_during("up", "1.1.0", "TZ=Europe/Paris\n");
    machine.fail("up", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains(
        "a changé pendant la mise à jour : vos changements restent, seuls les réglages ajoutés par la version en sont retirés : MODE",
    );
    run.assert_contains("version 1.0.0 relancée, saine");
    assert_eq!(
        machine.env_file(),
        format!("{before}\n# Mode\nTZ=Europe/Paris\n")
    );
}

/// Une version gardée que cet agent refuse ne fait jamais conseiller `rollback`, qui échouerait :
/// l'agent dit ce qui reste possible.
#[test]
fn a_refused_kept_version_is_not_advised_for_rollback() {
    let (machine, server) = installed();
    // La 1.0.0 a été installée par un agent plus ancien, qui admettait ce compose.
    fs::write(
        machine.release_dir("1.0.0").join("compose.yaml"),
        r#"{"services": {"app": {"image": "(forme publiée)", "env_file": "/etc/environment"}}}"#,
    )
    .unwrap();

    // `rollback: manual` : la version installée est contrôlée pour le conseil.
    machine.publish(&server, &Release::new("1.1.0"));
    machine.fail("up", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("La version 1.0.0 a été refusée");
    run.assert_contains("Cette version de drakbase-stack ne la relancera pas");
    run.assert_contains("reprendre la version 1.1.0 : sudo drakbase-stack update demo --retry");
    run.assert_contains("sudo drakbase-stack uninstall demo");
    assert!(
        !run.text().contains("drakbase-stack rollback"),
        "{}",
        run.text()
    );

    // `rollback` le dit aussi, sans rien relancer.
    machine.clear_logs();
    let run = machine.run(&["rollback", APP]);
    run.assert_code(1);
    run.assert_contains("compose de la version 1.0.0 refusé");
    run.assert_contains("Ce qui reste possible");
    run.assert_contains("sudo drakbase-stack update demo --retry");
    assert!(machine.applied().is_empty());

    // Le rappel d'`update` et le refus de `restart` ne conseillent pas `rollback` non plus.
    for command in [&["update", APP][..], &["restart", APP][..]] {
        let run = machine.run(command);
        run.assert_code(1);
        run.assert_contains(
            "la version 1.0.0 ne peut plus être relancée, son compose n'est plus admis",
        );
        run.assert_contains("reprendre la version 1.1.0 : sudo drakbase-stack update demo --retry");
        assert!(
            !run.text().contains("drakbase-stack rollback"),
            "{}",
            run.text()
        );
    }
    assert!(machine.applied().is_empty());

    // `rollback: safe` : le retour seul est refusé, et le conseil est le même.
    let mut safe = Release::new("1.2.0");
    safe.rollback = "safe";
    machine.publish(&server, &safe);
    machine.fail("up", "1.2.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("la version 1.0.0 n'a pas été relancée");
    run.assert_contains("reprendre la version 1.2.0 : sudo drakbase-stack update demo --retry");
    assert!(
        !run.text().contains("drakbase-stack rollback"),
        "{}",
        run.text()
    );
    assert_eq!(machine.applied(), ["pull 1.2.0"]);
}

/// Une version gardée n'est jamais relancée sans contrôle : avec un `.env` changé à la main,
/// la 1.0.0 demande une capacité de plus. Le retour seul s'arrête sans rien relancer, et
/// `rollback` aussi, jusqu'à `--accept`.
#[test]
fn a_kept_version_is_checked_before_it_is_relaunched() {
    let (machine, server) = installed();
    let mut widened = Release::new("1.0.0");
    widened.cap_add = vec!["NET_ADMIN".into()];
    machine.prepare_when(&server, "MODE=wide", &widened);
    fs::write(machine.stack_dir().join(".env"), "PORT=8080\nMODE=wide\n").unwrap();

    // Retour seul après l'échec d'une version qui, elle, n'a aucune permission nouvelle.
    let mut next = Release::new("1.1.0");
    next.rollback = "safe";
    machine.publish(&server, &next);
    machine.fail("up", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(3);
    run.assert_contains("la version 1.0.0 demande de nouvelles permissions");
    run.assert_contains("app : capacité NET_ADMIN");
    run.assert_contains("sudo drakbase-stack rollback demo --accept");
    assert_eq!(machine.applied(), ["pull 1.1.0"]);
    // Rien n'a été relancé : la 1.1.0 reste en place, en échec, comme avec `rollback: manual`.
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["failed"]["version"], "1.1.0");
    assert_eq!(state["rejected"], json!(["1.1.0"]));
    assert_eq!(
        state["accepted_permissions"],
        json!(["app|port|*:8080/tcp"])
    );
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);

    // `rollback`, depuis la version restée en échec.
    machine.clear_logs();
    let run = machine.run(&["rollback", APP]);
    run.assert_code(3);
    run.assert_contains("app : capacité NET_ADMIN");
    run.assert_contains("sudo drakbase-stack rollback demo --accept");
    assert!(machine.applied().is_empty());
    assert_eq!(machine.state()["failed"]["version"], "1.1.0");
    let run = machine.run(&["rollback", APP, "--accept"]);
    run.assert_code(0);
    run.assert_contains("Permissions acceptées (--accept).");
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    let state = machine.state();
    assert_eq!(state["failed"], json!(null));
    assert_eq!(
        state["accepted_permissions"],
        json!(["app|capability|NET_ADMIN", "app|port|*:8080/tcp"])
    );

    // `rollback`, depuis une version saine : la 1.0.0, précédente, demande un port de plus avec
    // le .env que la 1.2.0 a laissé.
    machine.publish(&server, &Release::new("1.2.0"));
    machine.run(&["update", APP]).assert_code(0);
    widened.ports = vec![8080, 9443];
    machine.prepare_when(&server, "MODE=wide", &widened);
    machine.clear_logs();
    let run = machine.run(&["rollback", APP]);
    run.assert_code(3);
    run.assert_contains("app : port 9443/tcp sur toutes les adresses");
    assert!(machine.applied().is_empty());
    assert_eq!(machine.state()["installed"]["version"], "1.2.0");
    machine.run(&["rollback", APP, "--accept"]).assert_code(0);
    assert_eq!(machine.applied(), ["up 1.0.0"]);
    assert_eq!(machine.state()["installed"]["version"], "1.0.0");
}

/// L'interface apprend qu'un retour seul ne s'est pas fait, et la commande à taper.
#[test]
fn the_interface_learns_that_the_return_alone_was_stopped() {
    let (machine, server) = installed_with_control();
    let mut widened = Release::with_control("1.0.0");
    widened.cap_add = vec!["NET_ADMIN".into()];
    machine.prepare_when(&server, "MODE=wide", &widened);
    fs::write(machine.stack_dir().join(".env"), "PORT=8080\nMODE=wide\n").unwrap();
    let mut next = Release::with_control("1.1.0");
    next.rollback = "safe";
    machine.publish(&server, &next);
    machine.run(&["check", APP]).assert_code(0);
    machine.fail("up", "1.1.0");

    machine.request(
        "update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    machine.run(&["request", APP]).assert_code(3);
    let status = machine.status();
    assert_eq!(status["installed"], "1.0.0");
    assert_eq!(status["failed"], "1.1.0");
    assert_eq!(status["operation"]["state"], "failed");
    let message = status["operation"]["message"].as_str().unwrap();
    assert!(
        message.contains("le retour à la 1.0.0 demande de nouvelles permissions")
            && message.contains("sudo drakbase-stack rollback demo --accept"),
        "{message}"
    );
}

/// Un dossier de contrôle créé pour une version qui s'arrête avant de démarrer part avec elle :
/// la version qui tourne n'en a pas.
#[test]
fn a_control_folder_made_for_a_version_that_does_not_start_is_removed() {
    let (machine, server) = installed();
    let control = machine.stack_dir().join("control");
    let mut next = Release::with_control("1.1.0");
    next.pre_update = true;
    machine.publish(&server, &next);
    for step in ["pull", "run"] {
        machine.fail(step, "1.1.0");
        machine.run(&["update", APP]).assert_code(1);
        assert!(!control.exists(), "{step}");
        machine.heal(step, "1.1.0");
    }
    // Une fois démarrée, la version le garde.
    machine.run(&["update", APP]).assert_code(0);
    assert_eq!(machine.status()["installed"], "1.1.0");
}

/// Retour seul vers une version sans dossier de contrôle, qui ne redevient pas saine : ce sont
/// ses conteneurs qui sont en place, et le dossier créé pour la version écartée part quand même.
#[test]
fn a_control_folder_made_for_a_discarded_version_goes_even_if_the_return_is_not_healthy() {
    let (machine, server) = installed();
    let control = machine.stack_dir().join("control");
    let mut next = Release::with_control("1.1.0");
    next.rollback = "safe";
    machine.publish(&server, &next);
    machine.fail("up", "1.1.0");
    machine.fail("up", "1.0.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("ne redevient pas saine non plus");
    assert!(!control.exists());
}

/// Un `.env.example` ne règle pas Docker Compose : la version est refusée avant que ses réglages
/// n'entrent dans le `.env` de la machine.
#[test]
fn settings_that_configure_docker_compose_are_refused() {
    let (machine, server) = installed();
    let before = machine.env_file();
    let mut next = Release::new("1.1.0");
    next.env_example = "# Port de l'interface\nPORT=8080\nCOMPOSE_PROFILES=drakbase-hooks\n".into();
    machine.publish(&server, &next);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("COMPOSE_PROFILES");
    run.assert_contains("env_name_reserved");
    assert_eq!(machine.env_file(), before);
    assert!(machine.applied().is_empty());

    // Une valeur sur deux lignes : Docker Compose la lirait, l'agent n'en recopierait que le
    // début dans le .env, qui ne se lirait plus.
    let mut next = Release::new("1.2.0");
    next.env_example = "PORT=8080\nCLE=\"debut\nfin\"\n".into();
    machine.publish(&server, &next);
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains(".env.example, ligne 2");
    run.assert_contains("env_line_unsupported");
    assert_eq!(machine.env_file(), before);
    assert!(machine.applied().is_empty());
}

/// Un rappel ne crée rien : sans `.env`, le conseil habituel est donné sans contrôler la version
/// installée, ce qui écrirait ce fichier.
#[test]
fn a_reminder_does_not_create_the_settings_file() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.fail("up", "1.1.0");
    machine.run(&["update", APP]).assert_code(1);
    let env = machine.stack_dir().join(".env");
    fs::remove_file(&env).unwrap();
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("sudo drakbase-stack rollback demo pour revenir à la version 1.0.0");
    assert!(!env.exists());
}

/// Une version restée en échec (`rollback: manual`) dont la version installée peut être relancée :
/// le rappel d'`update` et le refus de `restart` donnent la commande du retour.
#[test]
fn a_failed_version_is_recalled_with_a_return_that_works() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.fail("up", "1.1.0");
    machine.run(&["update", APP]).assert_code(1);
    for command in [&["update", APP][..], &["restart", APP][..]] {
        let run = machine.run(command);
        run.assert_code(1);
        run.assert_contains(
            "sudo drakbase-stack rollback demo pour revenir à la version 1.0.0, ou sudo drakbase-stack update demo --retry pour la reprendre",
        );
    }
}

/// Une demande déposée pendant que le dossier de la stack était ouvert à tous a pu venir d'un
/// compte de la machine : elle part sans être lue quand le dossier est refermé.
#[test]
fn a_request_dropped_while_the_stack_folder_was_open_is_discarded() {
    let (machine, server) = installed_with_control();
    let requests = machine.stack_dir().join("control/request");
    machine.publish(&server, &Release::with_control("1.1.0"));
    machine.run(&["check", APP]).assert_code(0);
    let operation = machine.status()["operation"].clone();
    machine.clear_logs();
    let checks = server.checks().len();

    fs::set_permissions(machine.stack_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    machine.request(
        "update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    fs::write(requests.join(".en-cours"), "{").unwrap();
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(mode(&machine.stack_dir()), 0o700);
    assert_eq!(fs::read_dir(&requests).unwrap().count(), 0);
    assert_eq!(server.checks().len(), checks);
    assert!(machine.applied().is_empty());
    assert_eq!(machine.status()["installed"], "1.0.0");
    assert_eq!(machine.status()["operation"], operation);

    // Dans le dossier fermé, la demande de l'app est traitée.
    machine.request(
        "update.json",
        r#"{"format": 1, "action": "update", "version": "1.1.0"}"#,
    );
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(machine.status()["installed"], "1.1.0");
}

/// `request` ne laisse pas de demande derrière un arrêt en erreur : l'unité le relancerait sans
/// fin. Quand l'état de la stack se lit, les demandes partent et l'app apprend l'échec ; sinon,
/// l'unité est désactivée.
#[test]
fn a_request_that_cannot_be_served_does_not_come_back() {
    let (machine, _server) = installed_with_control();
    machine.run(&["auto", "on"]).assert_code(0);
    let requests = machine.stack_dir().join("control/request");

    // Le `stack.json` de la version qui tourne ne se lit plus.
    let manifest = machine.release_dir("1.0.0").join("stack.json");
    let kept = fs::read(&manifest).unwrap();
    fs::write(&manifest, "{").unwrap();
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.clear_logs();
    let run = machine.run(&["request", APP]);
    run.assert_code(1);
    run.assert_contains("stack.json");
    assert_eq!(fs::read_dir(&requests).unwrap().count(), 0);
    let status = machine.status();
    assert_eq!(status["operation"]["action"], "check");
    assert_eq!(status["operation"]["state"], "failed");
    assert!(
        status["operation"]["message"]
            .as_str()
            .unwrap()
            .contains("stack.json")
    );
    // L'état se lit : l'unité reste, pour les demandes suivantes.
    assert!(machine.systemctl_calls().is_empty());
    fs::write(&manifest, kept).unwrap();

    // `state.json` ne se lit plus : rien ne peut être noté.
    fs::write(machine.stack_dir().join("state.json"), "{").unwrap();
    machine.request("check.json", r#"{"format": 1, "action": "check"}"#);
    machine.run(&["request", APP]).assert_code(1);
    assert_eq!(
        machine.systemctl_calls(),
        ["disable --now drakbase-stack-request@demo.path"]
    );
}

/// Un agent arrêté au milieu de la mise en place d'une version laisse son dossier de côté
/// (`.<version>.old`) : la commande suivante le remet avant de lire `releases/`.
#[test]
fn a_release_set_aside_by_an_interrupted_replacement_comes_back() {
    let (machine, server) = installed_with_control();
    machine.publish(&server, &Release::with_control("1.1.0"));
    machine.fail("up", "1.1.0");
    machine.run(&["update", APP]).assert_code(1);
    assert_eq!(machine.state()["failed"]["version"], "1.1.0");
    let releases = machine.stack_dir().join("releases");
    fs::rename(releases.join("1.1.0"), releases.join(".1.1.0.old")).unwrap();

    // `request` lit le `stack.json` de la version qui tourne, celle restée en échec.
    machine.run(&["request", APP]).assert_code(0);
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
    assert!(machine.release_dir("1.1.0").join("stack.json").is_file());

    // Arrêté après la mise en place : le dossier mis de côté part.
    fs::create_dir(releases.join(".1.1.0.old")).unwrap();
    machine.run(&["check", APP]).assert_code(1);
    assert_eq!(machine.releases(), ["1.0.0", "1.1.0"]);
}

#[test]
fn pull_or_preparation_failure_keeps_the_running_version() {
    let (machine, server) = installed();
    let mut next = Release::new("1.1.0");
    next.pre_update = true;
    machine.publish(&server, &next);

    machine.fail("pull", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("la version 1.0.0 continue de tourner");
    assert!(machine.applied().is_empty());

    machine.heal("pull", "1.1.0");
    machine.fail("run", "1.1.0");
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("préparation de la version 1.1.0");
    assert_eq!(machine.applied(), ["pull 1.1.0"]);

    // Ni l'un ni l'autre n'écarte la version : le prochain passage réessaie.
    let state = machine.state();
    assert_eq!(state["rejected"], json!([]));
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(machine.releases(), ["1.0.0"]);
    machine.heal("run", "1.1.0");
    machine.run(&["update", APP]).assert_code(0);
    assert_eq!(machine.state()["installed"]["version"], "1.1.0");
}

#[test]
fn failed_first_install_is_kept_for_retry_or_uninstall() {
    let machine = Machine::new();
    let server = FakeServer::start();
    machine.publish(&server, &Release::new("1.0.0"));
    machine.fail("up", "1.0.0");
    let run = machine.run(&[
        "install",
        APP,
        "--key",
        &key("test-app.pub"),
        "--server",
        &server.url,
        "--accept",
    ]);
    run.assert_code(1);
    run.assert_contains("sudo drakbase-stack uninstall demo");
    let state = machine.state();
    assert_eq!(state["installed"], json!(null));
    assert_eq!(state["failed"]["version"], "1.0.0");

    machine
        .run(&["rollback", APP])
        .assert_contains("aucune version saine");
    machine.heal("up", "1.0.0");
    machine.run(&["update", APP, "--retry"]).assert_code(0);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.0.0");
    assert_eq!(state["failed"], json!(null));
}

#[test]
fn uninstall_keeps_data_unless_asked() {
    let (machine, server) = installed();
    let _ = server;
    // --delete-data demande une confirmation dans un terminal.
    machine
        .run(&["uninstall", APP, "--delete-data"])
        .assert_code(2);
    assert!(machine.stack_dir().join("state.json").exists());

    let run = machine.run(&["uninstall", APP]);
    run.assert_code(0);
    run.assert_contains("ses volumes restent");
    let applied = machine.applied();
    assert_eq!(applied.len(), 1);
    assert!(
        applied[0].starts_with("down 1.0.0 down --remove-orphans"),
        "{applied:?}"
    );
    assert!(!applied[0].contains("--volumes"));
    assert!(has_call(&machine, "image rm"));
    assert!(!machine.stack_dir().join("state.json").exists());
    assert!(!machine.stack_dir().join("releases").exists());
    assert!(machine.stack_dir().join(".env").exists());

    // Le .env gardé resservira à une nouvelle installation.
    fs::write(machine.stack_dir().join(".env"), "PORT=9000\n").unwrap();
    machine.install(&server);
    assert_eq!(machine.env_file(), "PORT=9000\n");
}

/// Lance l'agent dans un pseudo-terminal, avec la commande `script` de macOS, et lui tape
/// `input` : `--delete-data` ne confirme que dans un terminal. Rend ce qui s'est affiché, ou
/// `None` hors de macOS ou sans `script` (l'essai est alors sauté, et le dit).
fn in_terminal(machine: &Machine, args: &[&str], input: &str) -> Option<String> {
    use std::io::{Read, Write};
    if !cfg!(target_os = "macos") {
        eprintln!(
            "essai sauté : le pseudo-terminal des essais passe par la commande script de macOS"
        );
        return None;
    }
    let agent = machine.agent();
    let mut command = Command::new("script");
    command
        .args(["-q", "/dev/null"])
        .arg(agent.get_program())
        .args(args);
    for (name, value) in agent.get_envs() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            eprintln!("essai sauté : commande script indisponible ({error})");
            return None;
        }
    };
    // La réponse est tapée quand la question est posée, et l'entrée reste ouverte jusqu'à la fin :
    // `script` de macOS fait passer la fin de l'entrée avant ce qui vient d'être tapé.
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut shown = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut answered = false;
    loop {
        let read = stdout.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        shown.extend_from_slice(&buffer[..read]);
        if !answered && String::from_utf8_lossy(&shown).contains("pour confirmer : ") {
            stdin.write_all(input.as_bytes()).unwrap();
            stdin.flush().unwrap();
            answered = true;
        }
    }
    drop(stdin);
    child.wait().unwrap();
    let shown = String::from_utf8_lossy(&shown).into_owned();
    Some(shown)
}

#[test]
fn delete_data_names_the_volumes_and_removes_them() {
    let (machine, _server) = installed();
    // La version qui tourne a aussi un volume à nom fixe, qu'une autre stack peut partager.
    let config = machine.docker_dir().join("config/1.0.0.json");
    let mut resolved: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    resolved["volumes"]["photos"] = json!({ "name": "photos" });
    fs::write(&config, serde_json::to_vec(&resolved).unwrap()).unwrap();

    // Le .env a été effacé à la main : l'agent le recrée pour lire le compose.
    let env = machine.stack_dir().join(".env");
    fs::remove_file(&env).unwrap();

    // Une autre réponse que le nom de l'app : rien ne part, et rien ne reste de plus.
    let Some(shown) = in_terminal(&machine, &["uninstall", APP, "--delete-data"], "oui\n") else {
        return;
    };
    assert!(!env.exists());
    assert!(
        shown.contains(
            "Volumes supprimés : demo_data, photos, et les volumes sans nom de ses conteneurs."
        ),
        "{shown}"
    );
    assert!(
        shown.contains("Attention : le volume photos a un nom fixe."),
        "{shown}"
    );
    assert!(
        shown.contains("Abandon : rien n'a été supprimé."),
        "{shown}"
    );
    assert!(machine.applied().is_empty());
    assert!(machine.stack_dir().join("state.json").exists());

    let shown = in_terminal(&machine, &["uninstall", APP, "--delete-data"], "demo\n").unwrap();
    assert!(shown.contains("désinstallée, données comprises"), "{shown}");
    assert_eq!(
        machine.applied(),
        ["down 1.0.0 down --remove-orphans --volumes"]
    );
    assert!(!machine.stack_dir().exists());
}

#[test]
fn auto_writes_systemd_units() {
    let machine = Machine::new();
    // Sans systemd, rien ne reste.
    let output = machine
        .agent()
        .env(
            "DRAKBASE_STACK_SYSTEMCTL",
            machine.root.path().join("absent"),
        )
        .args(["auto", "on"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("demandent systemd"));
    assert!(
        !machine
            .systemd_dir()
            .join("drakbase-stack.service")
            .exists()
    );

    let run = machine.run(&["auto", "on"]);
    run.assert_code(0);
    let service = fs::read_to_string(machine.systemd_dir().join("drakbase-stack.service")).unwrap();
    let exe = env!("CARGO_BIN_EXE_drakbase-stack");
    let quoted = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe.to_owned()
    };
    assert!(
        service.contains(&format!("ExecStart=-{quoted} self-update\n")),
        "{service}"
    );
    assert!(
        service.contains(&format!("ExecStart={quoted} check\n")),
        "{service}"
    );
    // Rien ne s'installe sans qu'une personne le demande.
    assert!(!service.contains(" update"), "{service}");
    let request = fs::read_to_string(
        machine
            .systemd_dir()
            .join("drakbase-stack-request@.service"),
    )
    .unwrap();
    assert!(
        request.contains(&format!("ExecStart={quoted} request %i\n")),
        "{request}"
    );
    assert!(
        machine
            .systemd_dir()
            .join("drakbase-stack-request@.path")
            .exists()
    );
    let timer = fs::read_to_string(machine.systemd_dir().join("drakbase-stack.timer")).unwrap();
    assert!(timer.contains("RandomizedDelaySec=1h"));
    assert!(timer.contains("Persistent=true"));
    assert_eq!(
        machine.systemctl_calls(),
        ["daemon-reload", "enable --now drakbase-stack.timer"]
    );
    machine
        .run(&["status"])
        .assert_contains("Vérification quotidienne : activée");
    // Une stack installée ensuite, dont l'app propose ses mises à jour dans son interface,
    // reçoit son unité des demandes.
    let server = FakeServer::start();
    machine.publish(&server, &Release::with_control("1.0.0"));
    machine.clear_logs();
    machine.install(&server);
    assert_eq!(
        machine.systemctl_calls(),
        ["enable --now drakbase-stack-request@demo.path"]
    );

    machine.clear_logs();
    machine.run(&["auto", "off"]).assert_code(0);
    assert!(!machine.systemd_dir().join("drakbase-stack.timer").exists());
    assert!(
        !machine
            .systemd_dir()
            .join("drakbase-stack.service")
            .exists()
    );
    assert!(
        !machine
            .systemd_dir()
            .join("drakbase-stack-request@.path")
            .exists()
    );
    assert_eq!(
        machine.systemctl_calls(),
        [
            "disable --now drakbase-stack.timer",
            "disable --now drakbase-stack-request@demo.path",
            "daemon-reload"
        ]
    );
    machine.run(&["auto", "peut-être"]).assert_code(2);
}

#[test]
fn status_and_logs() {
    let (machine, server) = installed();
    let run = machine.run(&["status"]);
    run.assert_code(0);
    run.assert_contains(&format!("Serveur : {}", server.url));
    run.assert_contains("demo 1.0.0 (canal stable)");
    run.assert_contains("services : app en marche et sain");

    let run = machine.run(&["logs", APP, "--tail", "20"]);
    run.assert_code(0);
    run.assert_contains("journal simulé de 1.0.0");
    assert!(has_call(&machine, "logs --tail 20"));
}

#[test]
fn the_compose_rules_apply_on_the_machine() {
    let (machine, server) = installed();
    // Le compose résolu désigne une image d'une autre app : refusé avant tout pull.
    let next = Release::new("1.1.0");
    machine.publish(&server, &next);
    let host = server.url.trim_start_matches("http://");
    let config = next.compose_json(host).replace("/demo/app@", "/autre/app@");
    fs::write(machine.docker_dir().join("config/1.1.0.json"), config).unwrap();
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("image_other_app");
    assert!(machine.applied().is_empty());
    assert_eq!(machine.releases(), ["1.0.0"]);
}

/// Le compose de l'archive est contrôlé tel qu'il est écrit, avant que Docker Compose ne le lise :
/// sa forme résolue, que rend ici le faux docker, ne montrerait rien de ce qu'il a lu.
#[test]
fn the_compose_of_the_archive_is_checked_as_written() {
    let (machine, server) = installed();
    let image = r#""image": "(forme publiée)""#;
    for (compose, code) in [
        (
            "services:\n  app:\n    image: x\n    env_file: /etc/environment\n".to_owned(),
            "compose_not_json",
        ),
        (
            format!(
                r#"{{"services": {{"app": {{{image}, "env_file": ["/var/lib/drakbase-stack/autre/.env"]}}}}}}"#
            ),
            "env_file_outside_files",
        ),
        (
            format!(
                r#"{{"services": {{"app": {{{image}, "extends": {{"file": "/etc/x.yaml", "service": "x"}}}}}}}}"#
            ),
            "compose_field_unsupported",
        ),
        (
            format!(r#"{{"include": ["/etc/x.yaml"], "services": {{"app": {{{image}}}}}}}"#),
            "compose_field_unsupported",
        ),
    ] {
        server.clear();
        machine.clear_logs();
        let mut next = Release::new("1.1.0");
        next.compose = Some(compose);
        machine.publish(&server, &next);
        let run = machine.run(&["update", APP]);
        run.assert_code(1);
        run.assert_contains(code);
        run.assert_contains("compose de la version 1.1.0 refusé");
        // Docker Compose n'a rien lu, et rien n'est resté de la version.
        assert!(!has_call(&machine, " config "), "{code}");
        assert_eq!(machine.releases(), ["1.0.0"], "{code}");
    }

    // Un env_file de files/ passe, comme le compose que publie l'outil.
    server.clear();
    let mut next = Release::new("1.1.0");
    next.compose = Some(format!(
        "{{\"services\": {{\"app\": {{{image}, \"env_file\": [{{\"path\": \"./files/app.env\", \"required\": true}}]}}}}}}\n"
    ));
    machine.publish(&server, &next);
    machine.run(&["update", APP]).assert_code(0);
}

/// Une version gardée repasse par le contrôle de son compose tel qu'il est écrit avant toute
/// relance : celle qu'un agent plus ancien a installée sans ce contrôle ne repart pas.
#[test]
fn a_kept_version_whose_compose_would_be_refused_is_not_relaunched() {
    let (machine, _server) = installed();
    let compose = machine.release_dir("1.0.0").join("compose.yaml");
    fs::write(
        &compose,
        r#"{"services": {"app": {"image": "(forme publiée)", "env_file": "/etc/environment"}}}"#,
    )
    .unwrap();
    let run = machine.run(&["restart", APP]);
    run.assert_code(1);
    run.assert_contains("compose de la version 1.0.0 refusé");
    run.assert_contains("env_file_outside_files");
    assert!(!has_call(&machine, " config "));
    assert!(machine.applied().is_empty());
}

#[test]
fn after_a_manual_failure_only_a_newer_version_goes_on_top() {
    let (machine, server) = installed();
    machine.publish(&server, &Release::new("1.2.0"));
    machine.fail("up", "1.2.0");
    machine.run(&["update", APP]).assert_code(1);
    assert_eq!(machine.state()["failed"]["version"], "1.2.0");

    // Une version plus ancienne que celle en échec ne s'installe pas par-dessus.
    server.clear();
    machine.publish(&server, &Release::new("1.1.0"));
    machine.clear_logs();
    let run = machine.run(&["update", APP]);
    run.assert_code(1);
    run.assert_contains("plus ancienne que la 1.2.0");
    assert!(machine.applied().is_empty());

    // Un correctif plus récent, si : il remplace la version en échec.
    machine.publish(&server, &Release::new("1.2.1"));
    machine.run(&["update", APP]).assert_code(0);
    let state = machine.state();
    assert_eq!(state["installed"]["version"], "1.2.1");
    assert_eq!(state["previous"]["version"], "1.0.0");
    assert_eq!(state["failed"], json!(null));
    // La 1.2.0 reste écartée : un rollback ramène à la 1.0.0, sous elle.
    assert_eq!(state["rejected"], json!(["1.2.0"]));
    assert_eq!(machine.releases(), ["1.0.0", "1.2.1"]);
    machine.run(&["rollback", APP]).assert_code(0);
    assert_eq!(machine.state()["rejected"], json!(["1.2.0", "1.2.1"]));
}
