//! Essais des stacks Docker et des apps cli par le binaire, contre le faux serveur : `init`,
//! `doctor`, `release --stack` et la publication d'exécutables `bin`. Les clés sont celles de
//! `fixtures/keys`, rangées dans le dossier temporaire de chaque essai : aucun ne lit ni n'écrit
//! le vrai trousseau ni une vraie base KeePassXC.
//!
//! Les essais qui construisent des images ont besoin de Docker, de buildx (avec un constructeur
//! qui produit des images OCI multi-plateformes) et de Docker Compose ; sans eux, ils sont sautés,
//! et le disent. Les images se construisent sans réseau : `FROM scratch`, puis `COPY`.

mod support;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use am_protocol::stack::archive;
use am_protocol::stack::manifest::StackManifest;
use am_protocol::{Arch, BundleType, Channel, Version};
use serde_json::{Value, json};

use support::fake_server::{FakeServer, State, served_archs};
use support::{both, drakbase, fixture_keys, fixture_keys_saved, login, public_key, run, stdout};

fn key_id(name: &str) -> String {
    am_protocol::signature::PublicKey::from_tauri(&public_key(name))
        .unwrap()
        .key_id()
        .to_hex()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Le binaire, lancé à la racine du projet. `HOME` est le dossier de l'essai : Docker y
/// chercherait sa configuration, ses greffons (`docker compose`, `docker buildx`) et ses
/// contextes. Il reçoit donc une configuration à lui, qui reprend le contexte courant et les
/// greffons de la vraie, sans rien y écrire.
fn in_project(home: &Path, project: &Path) -> Command {
    let mut command = drakbase(home);
    command.current_dir(project);
    let real = env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker")));
    if let Some(real) = real {
        let dir = home.join("docker-config");
        if !dir.exists() {
            fs::create_dir_all(&dir).unwrap();
            let mut config = json!({"cliPluginsExtraDirs": [real.join("cli-plugins")]});
            if let Some(current) = fs::read_to_string(real.join("config.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("currentContext").cloned())
            {
                config["currentContext"] = current;
            }
            fs::write(dir.join("config.json"), config.to_string()).unwrap();
            if real.join("contexts").is_dir() {
                std::os::unix::fs::symlink(real.join("contexts"), dir.join("contexts")).unwrap();
            }
        }
        command.env("DOCKER_CONFIG", &dir);
    }
    command
}

/// Le faux serveur, avec `test-app` de ce type et la clé d'essai principale active.
fn server_with(kind: &str) -> FakeServer {
    let mut state = State::demo(None);
    state.apps.get_mut("test-app").unwrap().kind = kind.to_owned();
    FakeServer::start(state)
}

/// Les clés d'essai comme clés de `test-app`, sauvegardées, et l'outil connecté au faux serveur.
fn ready_home(home: &Path, server: &FakeServer) {
    fixture_keys(home);
    fixture_keys_saved(home);
    let output = login(home, &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
}

fn docker(args: &[&str]) -> Option<Output> {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
}

/// La commande que conseille `release --stack` pour publier son brouillon (« Pour le publier :
/// drakbase publish … »), sans son premier mot : les arguments à donner au binaire.
fn advised_publish(report: &str) -> Vec<String> {
    let advice = report
        .lines()
        .find_map(|line| line.split_once("Pour le publier : "))
        .map(|(_, advice)| advice)
        .unwrap_or_else(|| panic!("aucune commande conseillée\n{report}"));
    let command = advice
        .split_once(" (rien ne sera")
        .map_or(advice, |(command, _)| command);
    let mut words = command.split_whitespace();
    assert_eq!(words.next(), Some("drakbase"), "{advice}");
    words.map(str::to_owned).collect()
}

/// Docker, buildx et Docker Compose.
fn tools_ready() -> bool {
    let ready = docker(&["version"]).is_some()
        && docker(&["buildx", "version"]).is_some()
        && docker(&["compose", "version"]).is_some();
    if !ready {
        eprintln!("Docker, buildx ou Docker Compose indisponible : essai sauté");
    }
    ready
}

/// De quoi construire : les outils, et un constructeur qui écrit des images OCI
/// multi-plateformes (le pilote docker ne le fait qu'avec le stockage d'images containerd).
fn build_ready() -> bool {
    if !tools_ready() {
        return false;
    }
    let builder = docker(&["buildx", "inspect"])
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let docker_driver = builder.lines().any(|line| {
        line.trim() == "Driver: docker" || line.trim().ends_with("Driver:        docker")
    });
    if docker_driver && !containerd_store() {
        eprintln!(
            "le constructeur de buildx (pilote docker, sans le stockage containerd) ne produit pas d'images OCI multi-plateformes : essai sauté"
        );
        return false;
    }
    true
}

fn containerd_store() -> bool {
    docker(&["info", "--format", "{{json .DriverStatus}}"])
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("containerd"))
}

/// Une stack d'essai : une image construite sur place (`FROM scratch` et un fichier), employée par
/// le service surveillé et par une sauvegarde lancée avant chaque mise à jour.
fn stack_project(dir: &Path, message: &str) {
    write(
        &dir.join("app/Dockerfile"),
        "FROM scratch\nCOPY hello.txt /hello.txt\n",
    );
    write(&dir.join("app/hello.txt"), message);
    write(
        &dir.join("stack/compose.yaml"),
        r#"name: essai-local
services:
  web:
    build: ../app
    image: demo-web:dev
    ports:
      - "${WEB_PORT:-8080}:80"
    environment:
      MESSAGE: "${MESSAGE:-bonjour}, 10 $$"
    volumes:
      - ./files/config.toml:/etc/demo/config.toml:ro
      - data:/data
  backup:
    image: demo-web:dev
    profiles: [drakbase-hooks]
    command: ["backup"]
    volumes:
      - data:/data
volumes:
  data:
"#,
    );
    write(&dir.join("stack/.env.example"), "WEB_PORT=8080\n");
    write(&dir.join("stack/files/config.toml"), "mode = \"essai\"\n");
}

/// Reconstitue, à partir de ce qu'a reçu le faux serveur, un dossier OCI que `docker load` lit,
/// puis vérifie que Docker en tire le fichier de l'image : les couches recompressées et les
/// manifestes réécrits sont bons pour Docker.
fn check_with_docker_load(state: &State, digest: &str, dir: &Path, expected: &str) {
    if !containerd_store() {
        eprintln!("stockage d'images containerd absent : vérification par docker load sautée");
        return;
    }
    let name = format!("drakbase-essai/demo-web:{}", &digest[7..19]);
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, path, data).unwrap();
    };
    add("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#);
    let index = &state.manifests["test-app"][digest];
    let top = json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": index.media_type,
            "digest": digest,
            "size": index.body.len(),
            "annotations": {
                "io.containerd.image.name": format!("docker.io/{name}"),
                "org.opencontainers.image.ref.name": &digest[7..19],
            },
        }],
    });
    add("index.json", top.to_string().as_bytes());
    for (blob, data) in &state.blobs["test-app"] {
        add(&format!("blobs/sha256/{}", &blob[7..]), data);
    }
    for (manifest, stored) in &state.manifests["test-app"] {
        add(&format!("blobs/sha256/{}", &manifest[7..]), &stored.body);
    }
    let layout = dir.join("layout.tar");
    fs::write(&layout, builder.into_inner().unwrap()).unwrap();
    let loaded = Command::new("docker")
        .args(["load", "-i"])
        .arg(&layout)
        .output()
        .unwrap();
    assert!(
        loaded.status.success(),
        "docker load : {}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    let created = Command::new("docker")
        .args(["create", &name, "/rien"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "docker load : {}\ndocker create : {}",
        String::from_utf8_lossy(&loaded.stdout),
        String::from_utf8_lossy(&created.stderr)
    );
    let container = String::from_utf8_lossy(&created.stdout).trim().to_owned();
    let copied = Command::new("docker")
        .args(["cp", &format!("{container}:/hello.txt"), "-"])
        .output()
        .unwrap();
    docker(&["rm", &container]);
    docker(&["image", "rm", &name]);
    assert!(copied.status.success(), "docker cp");
    let mut archive = tar::Archive::new(copied.stdout.as_slice());
    let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
    let mut contents = String::new();
    entry.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, expected);
}

#[test]
fn a_stack_is_set_up_built_sent_and_published() {
    if !build_ready() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("projet");
    stack_project(&project, "bonjour v1\n");
    let server = server_with("stack");
    ready_home(home.path(), &server);
    // L'outil publie par l'adresse du faux serveur, mais les machines tirent les images par
    // l'adresse publique que le serveur annonce : c'est elle qui entre dans les noms d'images et
    // dans la commande d'installation.
    let public = "https://maj.example.fr";
    server.state().public_url = Some(public.to_owned());

    // init : stack.json créé, service deviné dans le compose, clés écrites.
    let output = run(in_project(home.path(), &project).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "Stack Docker : stack",
        "✓ stack/stack.json : créé, avec le service surveillé « web » (à vérifier) et les plateformes linux/arm64 et linux/amd64, clés publiques (public_keys), la principale puis celle de secours",
        &format!(
            "✓ app « Test App » sur le serveur, clé {} active",
            key_id("test-app")
        ),
        "drakbase release --app test-app --stack stack --version <version> --channel stable",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let stack_json: Value =
        serde_json::from_str(&fs::read_to_string(project.join("stack/stack.json")).unwrap())
            .unwrap();
    assert_eq!(
        stack_json,
        json!({
            "format": 1,
            "service": "web",
            "platforms": ["linux/arm64", "linux/amd64"],
            "public_keys": [public_key("test-app"), public_key("other-app")],
        })
    );
    // Le développeur déclare sa sauvegarde.
    let mut edited = stack_json.clone();
    edited["pre_update"] = json!("backup");
    write(
        &project.join("stack/stack.json"),
        &serde_json::to_string_pretty(&edited).unwrap(),
    );

    let output = run(in_project(home.path(), &project).args(["doctor", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ stack Docker : stack",
        "✓ stack/stack.json : service web, plateformes linux/arm64 et linux/amd64, retour arrière manual, backup avant chaque mise à jour",
        &format!(
            "✓ clé embarquée en premier (public_keys de stack.json) : {}, la clé principale",
            key_id("test-app")
        ),
        "✓ compose lu par Docker Compose : 1 image(s) à construire, demo-web (backup, web)",
        "✓ compose publié conforme aux règles des stacks",
        "· demandé à la machine : web : port 8080/tcp sur toutes les adresses",
        "✓ app « Test App » sur le serveur",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("Signature de code"), "{text}");

    // release : construction, recompression, brouillon, images, archives.
    write(&project.join("notes.md"), "- Première version\n");
    let release = |extra: &[&str]| {
        run(in_project(home.path(), &project)
            .args([
                "release",
                "--app",
                "test-app",
                "--stack",
                "--version",
                "1.0",
                "--channel",
                "stable",
                "--notes",
                "notes.md",
            ])
            .args(extra))
    };
    let output = release(&[]);
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    let host = public.trim_start_matches("https://");
    for expected in [
        "Stack   : stack (service web, plateformes linux/arm64 et linux/amd64)",
        "· image demo-web : services backup, web (construite par web)",
        "Recompression des couches de l'image demo-web :",
        "en zstd -19",
        "✓ compose publié conforme aux règles des stacks",
        "  · web : port 8080/tcp sur toutes les adresses",
        "✓ Archives : dist/test-app-1.0.0-aarch64.stack.tar.gz, dist/test-app-1.0.0-x86_64.stack.tar.gz",
        "✓ Signature : dist/test-app-1.0.0-x86_64.stack.tar.gz.sig, clé",
        &format!(
            "Clés embarquées par cette version : {}, {}",
            key_id("test-app"),
            key_id("other-app")
        ),
        "Brouillon 1.0.0 créé sur le serveur.",
        "✓ linux/aarch64/stack : envoyé",
        "Pour le publier : drakbase publish --app test-app --version 1.0.0 --channel stable --publish dist/test-app-1.0.0-aarch64.stack.tar.gz dist/test-app-1.0.0-x86_64.stack.tar.gz",
        &format!("  curl -fsSL {public}/install/drakbase-stack.sh | sudo sh"),
        &format!(
            "  sudo drakbase-stack install test-app --key {} --server {public}",
            public_key("test-app"),
        ),
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    let state = server.state();
    let release_state = &state.apps["test-app"].releases["1.0.0"];
    assert_eq!(release_state.status, "draft");
    let slots: BTreeSet<String> = release_state
        .artifacts
        .iter()
        .map(|artifact| artifact.slot())
        .collect();
    assert_eq!(
        slots,
        BTreeSet::from([
            "linux/aarch64/stack".to_owned(),
            "linux/x86_64/stack".to_owned()
        ])
    );
    let mut trusted = release_state.trusted_key_ids.clone();
    trusted.sort();
    let mut expected_keys = vec![key_id("test-app"), key_id("other-app")];
    expected_keys.sort();
    assert_eq!(trusted, expected_keys);
    let digest = release_state.images["demo-web"].clone();
    assert_eq!(
        served_archs(&state, "test-app", &digest),
        BTreeSet::from([Arch::Aarch64, Arch::X86_64])
    );
    // Toutes les couches en zstd.
    for stored in state.manifests["test-app"].values() {
        let manifest: Value = serde_json::from_slice(&stored.body).unwrap();
        for layer in manifest["layers"].as_array().into_iter().flatten() {
            assert_eq!(
                layer["mediaType"],
                "application/vnd.oci.image.layer.v1.tar+zstd"
            );
        }
    }
    // Les couches, puis les manifestes, puis les archives.
    let paths: Vec<String> = state
        .requests
        .iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    let last_blob = paths
        .iter()
        .rposition(|p| p.starts_with("PUT") && p.contains("/blobs/"));
    let first_manifest = paths
        .iter()
        .position(|p| p.starts_with("PUT") && p.contains("/manifests/"));
    let last_manifest = paths
        .iter()
        .rposition(|p| p.starts_with("PUT") && p.contains("/manifests/"));
    let first_archive = paths.iter().position(|p| p.contains("/artifacts/linux/"));
    assert!(
        last_blob < first_manifest && last_manifest < first_archive,
        "{paths:#?}"
    );
    let index_put = format!("PUT /api/v1/publish/apps/test-app/manifests/{digest}");
    assert_eq!(paths.iter().rposition(|p| *p == index_put), last_manifest);
    let requests_before = state.requests.len();
    drop(state);

    // L'archive : stack.json publié, compose épinglé, réglages et fichiers.
    let bytes = fs::read(project.join("dist/test-app-1.0.0-aarch64.stack.tar.gz")).unwrap();
    assert_eq!(
        bytes,
        fs::read(project.join("dist/test-app-1.0.0-x86_64.stack.tar.gz")).unwrap()
    );
    let read = archive::read(&bytes).unwrap();
    assert_eq!(
        read.files().keys().collect::<Vec<_>>(),
        [
            ".env.example",
            "compose.yaml",
            "files/config.toml",
            "stack.json"
        ]
    );
    let version = Version::parse_release("1.0.0").unwrap();
    let published =
        StackManifest::parse_published(read.stack_json(), "test-app", &version).unwrap();
    assert_eq!(published.images["demo-web"].as_str(), digest);
    assert_eq!(published.pre_update.as_deref(), Some("backup"));
    let compose = String::from_utf8(read.compose_yaml().to_vec()).unwrap();
    let pinned = format!("\"image\": \"{host}/test-app/demo-web@{digest}\"");
    assert_eq!(compose.matches(&pinned).count(), 2, "{compose}");
    for kept in [
        "${WEB_PORT:-8080}:80",
        "${MESSAGE:-bonjour}, 10 $$",
        "./files/config.toml",
    ] {
        assert!(compose.contains(kept), "{kept}\n{compose}");
    }
    for removed in ["\"build\"", "essai-local", "dev\""] {
        assert!(!compose.contains(removed), "{removed}\n{compose}");
    }
    let signature =
        fs::read_to_string(project.join("dist/test-app-1.0.0-x86_64.stack.tar.gz.sig")).unwrap();
    let decoded = String::from_utf8(
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, signature.trim())
            .unwrap(),
    )
    .unwrap();
    assert!(
        decoded.contains("file:test-app-1.0.0-x86_64.stack.tar.gz\tversion:1.0.0"),
        "{decoded}"
    );

    // Publication : la même commande, rien ne repart.
    let output = release(&["--publish"]);
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "Brouillon 1.0.0 repris",
        &format!("✓ image demo-web : {digest}"),
        "tout était déjà sur le serveur",
        "· linux/x86_64/stack : déjà sur le serveur, identique, pas renvoyé",
        "✓ Version 1.0.0 publiée sur le canal stable.",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let state = server.state();
    assert!(
        state.requests[requests_before..]
            .iter()
            .all(|request| request.method != "PUT"),
        "rien ne doit être renvoyé"
    );
    assert_eq!(state.apps["test-app"].releases["1.0.0"].status, "published");
    assert_eq!(
        state.apps["test-app"].releases["1.0.0"].channel,
        Channel::Stable
    );

    // Docker lit les images telles qu'elles seront servies.
    check_with_docker_load(&state, &digest, home.path(), "bonjour v1\n");
    drop(state);

    // Version suivante, en brouillon, avec ses notes : seule la couche qui change part.
    write(&project.join("app/hello.txt"), "bonjour v2\n");
    write(&project.join("notes-1.1.md"), "- Deuxième version\n");
    let before = server.state().requests.len();
    let output = run(in_project(home.path(), &project).args([
        "release",
        "--app",
        "test-app",
        "--stack",
        "stack",
        "--version",
        "1.1.0",
        "--channel",
        "stable",
        "--notes",
        "notes-1.1.md",
    ]));
    assert!(output.status.success(), "{}", both(&output));
    let advised = advised_publish(&stdout(&output));
    let state = server.state();
    let new_blobs = state.requests[before..]
        .iter()
        .filter(|request| request.method == "PUT" && request.path.contains("/blobs/"))
        .count();
    // La nouvelle couche (la même pour les deux archs) et deux configurations.
    assert_eq!(new_blobs, 3, "{}", stdout(&output));
    let draft = &state.apps["test-app"].releases["1.1.0"];
    assert_ne!(draft.images["demo-web"], digest);
    assert_eq!(draft.status, "draft");
    assert_eq!(draft.notes_md, "- Deuxième version\n");
    let requests_before = state.requests.len();
    drop(state);

    // Le brouillon se publie par la commande que conseille release, qui n'a pas de --notes : rien
    // n'est reconstruit ni renvoyé, et la version garde les notes du brouillon.
    assert_eq!(
        advised,
        [
            "publish",
            "--app",
            "test-app",
            "--version",
            "1.1.0",
            "--channel",
            "stable",
            "--publish",
            "dist/test-app-1.1.0-aarch64.stack.tar.gz",
            "dist/test-app-1.1.0-x86_64.stack.tar.gz",
        ]
    );
    let output = run(in_project(home.path(), &project).args(&advised));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "Brouillon 1.1.0 repris",
        "· linux/aarch64/stack : déjà sur le serveur, identique, pas renvoyé",
        "✓ Version 1.1.0 publiée sur le canal stable.",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let state = server.state();
    assert!(
        state.requests[requests_before..]
            .iter()
            .all(|request| request.method != "PUT"),
        "rien ne doit être renvoyé"
    );
    assert_eq!(
        state.release_bodies.last().unwrap().get("notes_md"),
        None,
        "notes_md ne doit pas partir sans --notes"
    );
    let published = &state.apps["test-app"].releases["1.1.0"];
    assert_eq!(published.status, "published");
    assert_eq!(published.notes_md, "- Deuxième version\n");
}

#[test]
fn a_stack_is_set_up_and_checked_without_docker() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("projet");
    stack_project(&project, "bonjour\n");
    let server = server_with("stack");
    ready_home(home.path(), &server);
    let absent = home.path().join("docker-absent");
    let without_docker = |args: &[&str]| {
        run(in_project(home.path(), &project)
            .env("APPSMARKET_DOCKER", &absent)
            .args(args))
    };

    // Sans Docker Compose pour lire le compose, le service surveillé prend le nom de l'app.
    let output = without_docker(&["init", "test-app", "--stack", "stack"]);
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "! service surveillé : test-app, faute de lire le compose",
        "introuvable : installe Docker",
        "✓ stack/stack.json : créé, avec le service surveillé « test-app »",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let path = project.join("stack/stack.json");
    let created = fs::read_to_string(&path).unwrap();
    let output = without_docker(&["init", "test-app"]);
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("· stack/stack.json : déjà en place"),
        "{}",
        both(&output)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), created);

    let output = without_docker(&["doctor", "test-app"]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        format!(
            "✓ clé embarquée en premier (public_keys de stack.json) : {}, la clé principale",
            key_id("test-app")
        ),
        "✓ stack/.env.example : les réglages de la machine".to_owned(),
        format!("✗ {} introuvable : installe Docker", absent.display()),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    // Sans l'app, doctor ne peut pas la deviner : stack.json ne la dit pas.
    let output = without_docker(&["doctor"]);
    assert!(
        stdout(&output).contains("✗ impossible de savoir quelle app contrôler"),
        "{}",
        both(&output)
    );

    // Une app d'un autre type, puis Docker absent : refus avant toute construction.
    server.state().apps.get_mut("test-app").unwrap().kind = "swift".to_owned();
    let release = &[
        "release",
        "--app",
        "test-app",
        "--stack",
        "--version",
        "1.0.0",
        "--channel",
        "beta",
    ];
    let output = without_docker(release);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        both(&output)
            .contains("l'app test-app est une app Swift sur le serveur, pas une stack Docker"),
        "{}",
        both(&output)
    );
    server.state().apps.get_mut("test-app").unwrap().kind = "stack".to_owned();
    let output = without_docker(release);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        both(&output).contains("introuvable : installe Docker"),
        "{}",
        both(&output)
    );
    assert!(server.state().apps["test-app"].releases.is_empty());
    assert!(!project.join("dist").exists());

    // Les options : --version avec --stack, pas de bundle avec --stack.
    let output = without_docker(&[
        "release",
        "--app",
        "test-app",
        "--stack",
        "--channel",
        "beta",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", both(&output));
    let output = without_docker(&[
        "release",
        "Demo.app",
        "--app",
        "test-app",
        "--stack",
        "--version",
        "1.0.0",
        "--channel",
        "beta",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", both(&output));

    // Une autre clé déjà écrite n'est jamais remplacée.
    let mut other: Value = serde_json::from_str(&created).unwrap();
    other["public_keys"] = json!([public_key("other-app")]);
    write(&path, &other.to_string());
    let output = without_docker(&["init", "test-app"]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        both(&output).contains("embarque déjà d'autres clés (public_keys"),
        "{}",
        both(&output)
    );
}

#[test]
fn a_compose_that_breaks_a_rule_stops_release_before_building() {
    if !tools_ready() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("projet");
    stack_project(&project, "bonjour\n");
    write(
        &project.join("stack/stack.json"),
        &json!({"format": 1, "service": "web", "pre_update": "backup", "public_keys": [public_key("test-app")]})
            .to_string(),
    );
    let server = server_with("stack");
    ready_home(home.path(), &server);
    let compose = fs::read_to_string(project.join("stack/compose.yaml")).unwrap();
    let release = || {
        run(in_project(home.path(), &project).args([
            "release",
            "--app",
            "test-app",
            "--stack",
            "--version",
            "1.0.0",
            "--channel",
            "stable",
        ]))
    };
    for (broken, expected) in [
        // Un dossier du projet monté hors de files/ : il ne serait pas dans l'archive.
        (
            compose.replace("- data:/data\n  backup", "- ./donnees:/data\n  backup"),
            "(bind_outside_files)",
        ),
        // Une variable pour source, en syntaxe courte.
        (
            compose.replace(
                "- data:/data\n  backup",
                "- ${DATA_DIR:-/srv/demo}:/data\n  backup",
            ),
            "prend une variable pour source",
        ),
        // Le service surveillé avec un profil.
        (
            compose.replace("    build: ../app", "    build: ../app\n    profiles: [x]"),
            "(profile_unexpected)",
        ),
    ] {
        write(&project.join("stack/compose.yaml"), &broken);
        let output = release();
        assert_eq!(output.status.code(), Some(1), "{}", both(&output));
        assert!(
            both(&output).contains(expected),
            "{expected}\n{}",
            both(&output)
        );
        assert!(!both(&output).contains("Construction"), "{}", both(&output));
    }
    assert!(server.state().image_requests().is_empty());
    assert!(server.state().apps["test-app"].releases.is_empty());
    assert!(!project.join("dist").exists());
}

/// `publish` d'une archive de stack contrôle son compose tel qu'il est écrit, comme le fera
/// l'agent : une archive qu'il refuserait ne part pas.
#[test]
fn publish_refuses_an_archive_whose_compose_the_agent_would_refuse() {
    let home = tempfile::tempdir().unwrap();
    let server = server_with("stack");
    ready_home(home.path(), &server);
    let digest = format!("sha256:{}", "1".repeat(64));
    let image = format!("\"image\": \"maj.example.fr/test-app/app@{digest}\"");
    let manifest = json!({
        "format": 1,
        "app": "test-app",
        "version": "1.0.0",
        "service": "app",
        "public_keys": [public_key("test-app")],
        "images": { "app": digest },
    })
    .to_string();
    let path = home.path().join("test-app-1.0.0-aarch64.stack.tar.gz");
    let publish = |compose: String| {
        let files = std::collections::BTreeMap::from([
            ("stack.json".to_owned(), manifest.clone().into_bytes()),
            ("compose.yaml".to_owned(), compose.into_bytes()),
        ]);
        fs::write(&path, archive::write(&files).unwrap()).unwrap();
        support::sign(home.path(), &path, "1.0.0", "test-app");
        run(drakbase(home.path())
            .args(["publish", "--app", "test-app", "--version", "1.0.0"])
            .args(["--channel", "stable"])
            .arg(&path))
    };

    for (compose, code) in [
        (
            format!("services:\n  app:\n    image: maj.example.fr/test-app/app@{digest}\n"),
            "compose_not_json",
        ),
        (
            format!(
                "{{\"services\": {{\"app\": {{{image}, \"env_file\": \"/var/lib/drakbase-stack/autre/.env\"}}}}}}"
            ),
            "env_file_outside_files",
        ),
        (
            format!(
                "{{\"services\": {{\"app\": {{{image}, \"extends\": {{\"file\": \"/etc/x.yaml\", \"service\": \"x\"}}}}}}}}"
            ),
            "compose_field_unsupported",
        ),
    ] {
        let output = publish(compose);
        assert!(!output.status.success(), "{code}\n{}", both(&output));
        assert!(both(&output).contains(code), "{code}\n{}", both(&output));
        assert!(server.state().uploads().is_empty(), "{code}");
    }

    // Le compose que publie l'outil, avec un env_file de files/ : l'archive part. Le faux serveur
    // la refuse ensuite, faute d'image, mais ce n'est plus l'affaire du compose.
    let output = publish(format!(
        "{{\"services\": {{\"app\": {{{image}, \"env_file\": [\"./files/app.env\"]}}}}}}\n"
    ));
    assert_eq!(server.state().uploads().len(), 1, "{}", both(&output));
    assert!(!both(&output).contains("compose"), "{}", both(&output));
}

/// Un exécutable ELF réduit à son en-tête : 64 bits, petit-boutien, de cette machine.
fn elf(machine: u16) -> Vec<u8> {
    let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
    bytes.resize(18, 0);
    // `e_type` : un exécutable.
    bytes[16] = 2;
    bytes.extend(machine.to_le_bytes());
    bytes.resize(64, 0);
    bytes
}

#[test]
fn a_cli_app_declares_the_keys_of_drakbase_json() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("agent");
    write(
        &project.join("Cargo.toml"),
        "[package]\nname = \"agent\"\nversion = \"0.4.0\"\n",
    );
    let server = server_with("cli");
    ready_home(home.path(), &server);

    let output = run(in_project(home.path(), &project).args(["init", "test-app", "--cli"]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "App cli   : drakbase.json",
        "✓ drakbase.json : créé, app test-app, clés publiques (public_keys), la principale puis celle de secours",
        "drakbase publish --app test-app --version <version> --channel stable --notes notes.md --publish linux/aarch64/bin=",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(project.join("drakbase.json")).unwrap()).unwrap();
    assert_eq!(
        manifest,
        json!({"app": "test-app", "public_keys": [public_key("test-app"), public_key("other-app")]})
    );

    // doctor reconnaît le projet par drakbase.json, qui dit aussi l'app.
    let output = run(in_project(home.path(), &project).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ app cli : drakbase.json",
        "✓ app : test-app",
        "✓ drakbase.json : app test-app",
        "✓ version du projet : 0.4.0",
        "Tout est prêt.",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    // Les exécutables, reconnus à leur en-tête ELF, signés puis publiés.
    let dist = project.join("dist");
    fs::create_dir_all(&dist).unwrap();
    let arm = dist.join("agent-0.4.0-aarch64");
    let intel = dist.join("agent");
    fs::write(&arm, elf(0xb7)).unwrap();
    fs::write(&intel, elf(0x3e)).unwrap();
    for file in [&arm, &intel] {
        support::sign(home.path(), file, "0.4.0", "test-app");
    }
    let publish = |args: &[&str]| {
        run(in_project(home.path(), &project)
            .args([
                "publish",
                "--app",
                "test-app",
                "--version",
                "0.4.0",
                "--channel",
                "stable",
            ])
            .args(args))
    };
    let path = |file: &PathBuf| file.display().to_string();

    let output = publish(&[&format!("linux/x86_64/bin={}", path(&arm))]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        both(&output).contains("l'exécutable est fait pour aarch64, pas pour l'arch x86_64"),
        "{}",
        both(&output)
    );
    let output = publish(&[&format!(
        "linux/x86_64/bin={}",
        project.join("Cargo.toml").display()
    )]);
    assert!(
        both(&output).contains("n'est pas un exécutable Linux (ELF)"),
        "{}",
        both(&output)
    );

    let output = publish(&["--publish", &path(&arm), &path(&intel)]);
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ linux/aarch64/bin : agent-0.4.0-aarch64",
        "✓ linux/x86_64/bin : agent",
        &format!(
            "Clés embarquées par cette version : {}, {} (d'après drakbase.json)",
            key_id("test-app"),
            key_id("other-app")
        ),
        "✓ Version 0.4.0 publiée sur le canal stable.",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let state = server.state();
    let release = &state.apps["test-app"].releases["0.4.0"];
    assert_eq!(
        release.trusted_key_ids,
        [key_id("test-app"), key_id("other-app")]
    );
    assert!(
        release
            .artifacts
            .iter()
            .all(|artifact| artifact.bundle_type == BundleType::Bin)
    );
    drop(state);

    // Sans drakbase.json, la version ne peut pas être publiée : ses clés seraient inconnues.
    let elsewhere = home.path().join("ailleurs");
    fs::create_dir_all(&elsewhere).unwrap();
    let next = dist.join("agent-0.5.0-aarch64");
    fs::write(&next, elf(0xb7)).unwrap();
    support::sign(home.path(), &next, "0.5.0", "test-app");
    let output = run(in_project(home.path(), &elsewhere).args([
        "publish",
        "--app",
        "test-app",
        "--version",
        "0.5.0",
        "--channel",
        "stable",
        "--publish",
        &path(&next),
    ]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        both(&output).contains("--drakbase-json"),
        "{}",
        both(&output)
    );
    let output = run(in_project(home.path(), &elsewhere).args([
        "publish",
        "--app",
        "test-app",
        "--version",
        "0.5.0",
        "--channel",
        "stable",
        "--drakbase-json",
        &project.display().to_string(),
        &path(&next),
    ]));
    assert!(output.status.success(), "{}", both(&output));
}
