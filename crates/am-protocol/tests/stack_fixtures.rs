//! Rejoue les jeux d'essai des stacks Docker (`fixtures/stack/`) : `stack.json`, archives,
//! analyse du compose et manifestes OCI. Ces règles ne sont implémentées qu'en Rust (serveur,
//! outil, agent) : le paquet Swift n'a pas à les rejouer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::Version;
use am_protocol::signature::{PublicKey, Signature};
use am_protocol::stack::compose::{self, ComposeContext, ComposeReport, Permission};
use am_protocol::stack::control::{Request, Status};
use am_protocol::stack::manifest::StackManifest;
use am_protocol::stack::oci::{self, OciObject};
use am_protocol::stack::{Digest, StackError, archive, docker_platform};
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn read_json(name: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(fixtures().join(name)).unwrap()).unwrap()
}

fn cases(name: &str) -> Vec<Value> {
    read_json(name)["cases"].as_array().unwrap().clone()
}

/// Remplace les chaînes `@<chemin>` par le contenu du fichier de `fixtures/`.
fn substitute(value: &Value) -> Value {
    match value {
        Value::String(text) if text.starts_with('@') => Value::String(
            fs::read_to_string(fixtures().join(&text[1..]))
                .unwrap()
                .trim()
                .to_owned(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(substitute).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Corps d'un cas : `raw` (texte), `file` (fichier de fixtures/) ou `document` (JSON).
fn body(case: &Value) -> Vec<u8> {
    if let Some(raw) = case["raw"].as_str() {
        raw.as_bytes().to_vec()
    } else if let Some(file) = case["file"].as_str().or(case["config"].as_str()) {
        fs::read(fixtures().join(file)).unwrap()
    } else {
        serde_json::to_vec(&substitute(&case["document"])).unwrap()
    }
}

fn outcome<T>(result: &Result<T, StackError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(error) => error.code(),
    }
}

fn key_hex(path: &str) -> String {
    let text = fs::read_to_string(fixtures().join(path)).unwrap();
    PublicKey::from_tauri(text.trim())
        .unwrap()
        .key_id()
        .to_hex()
}

#[test]
fn manifest_cases() {
    for case in cases("stack/manifests.json") {
        let name = case["name"].as_str().unwrap();
        let bytes = body(&case);
        let app = case["expect_app"].as_str().unwrap_or("demo");
        let version =
            Version::parse_release(case["expect_version"].as_str().unwrap_or("1.0.0")).unwrap();
        let result = match case["mode"].as_str().unwrap() {
            "source" => StackManifest::parse_source(&bytes),
            "published" => StackManifest::parse_published(&bytes, app, &version),
            other => panic!("mode {other}"),
        };
        assert_eq!(
            outcome(&result),
            case["expected"].as_str().unwrap(),
            "{name} : {result:?}"
        );
        let Ok(manifest) = result else { continue };
        let check = &case["check"];
        assert_eq!(
            u64::from(manifest.health_timeout),
            check["health_timeout"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(
            manifest.rollback.as_str(),
            check["rollback"].as_str().unwrap(),
            "{name}"
        );
        let platforms: Vec<&str> = manifest
            .platforms
            .iter()
            .map(|arch| docker_platform(*arch).unwrap())
            .collect();
        let expected: Vec<&str> = check["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(platforms, expected, "{name}");
        let keys: Vec<String> = manifest
            .trusted_key_ids()
            .iter()
            .map(|id| id.to_hex())
            .collect();
        let expected_keys: Vec<String> = check["trusted_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| key_hex(&v.as_str().unwrap()[1..]))
            .collect();
        assert_eq!(keys, expected_keys, "{name}");
        assert_eq!(
            manifest.app_control.as_deref(),
            check["app_control"].as_str(),
            "{name}"
        );
        let images: Vec<&String> = manifest.images.keys().collect();
        let expected_images: Vec<&str> = check["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(images, expected_images, "{name}");

        // Ce que l'outil écrit se relit à l'identique, dans la lecture qui lui correspond.
        let written = manifest.to_json();
        let reread = match &manifest.app {
            Some(app) => StackManifest::parse_published(
                written.as_bytes(),
                app,
                manifest.version.as_ref().unwrap(),
            ),
            None => StackManifest::parse_source(written.as_bytes()),
        };
        assert_eq!(
            reread.as_ref(),
            Ok(&manifest),
            "{name} : relecture de\n{written}"
        );
    }
}

#[test]
fn source_then_published() {
    let source = br#"{"format": 1, "service": "app", "public_keys": ["KEY"]}"#;
    let key = fs::read_to_string(fixtures().join("keys/test-app.pub")).unwrap();
    let source = String::from_utf8_lossy(source).replace("KEY", key.trim());
    let manifest = StackManifest::parse_source(source.as_bytes()).unwrap();
    let version = Version::parse_release("1.2.0").unwrap();
    let images = BTreeMap::from([("app".to_owned(), Digest::of(b"index"))]);
    let published = manifest.published("demo", &version, images).to_json();
    let reread = StackManifest::parse_published(published.as_bytes(), "demo", &version).unwrap();
    assert_eq!(reread.images["app"], Digest::of(b"index"));
    assert!(published.ends_with("}\n"));
    assert_eq!(
        StackManifest::parse_source(published.as_bytes())
            .unwrap_err()
            .code(),
        "field_tool_only"
    );
}

#[test]
fn archive_cases() {
    for case in cases("stack/archives.json") {
        let name = case["name"].as_str().unwrap();
        let result = archive::read(&body(&case));
        assert_eq!(
            outcome(&result),
            case["expected"].as_str().unwrap(),
            "{name} : {result:?}"
        );
        if let Ok(read) = result {
            // Relire puis réécrire une archive valide donne des fichiers identiques.
            let rewritten = archive::write(read.files()).unwrap();
            assert_eq!(archive::read(&rewritten).unwrap(), read, "{name}");
        }
    }
}

/// L'archive publiée d'essai : signature, version signée, contenu et `stack.json`.
#[test]
fn signed_demo_archive() {
    let name = "demo-1.0.0-aarch64.stack.tar.gz";
    let bytes = fs::read(fixtures().join("stack").join(name)).unwrap();
    let key = PublicKey::from_tauri(
        fs::read_to_string(fixtures().join("keys/test-app.pub"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    let signature = Signature::from_tauri(
        &fs::read_to_string(fixtures().join(format!("stack/{name}.sig"))).unwrap(),
    )
    .unwrap();
    let version = Version::parse_release("1.0.0").unwrap();
    let verified = key
        .verify(&bytes, &signature)
        .expect("archive réécrite différemment : supprimer le .sig, puis relancer gen_fixtures");
    verified.require_version(&version).unwrap();
    assert_eq!(verified.field("file"), Some(name));

    let read = archive::read(&bytes).unwrap();
    assert_eq!(
        archive::write(read.files()).unwrap(),
        bytes,
        "archive reproductible"
    );
    let manifest = StackManifest::parse_published(read.stack_json(), "demo", &version).unwrap();
    assert_eq!(manifest.trusted_key_ids(), vec![key.key_id()]);
    assert!(read.env_example().is_some());
    assert!(read.files().contains_key("files/app.conf"));
}

/// L'exemple `examples/stack-demo` suit le contrat : son `stack.json` se lit, une fois les clés
/// ajoutées comme le fait `drakbase init` ; ses fichiers font une archive ; et chaque réglage de
/// `.env.example` est lu par le compose, puis affiché par l'image.
#[test]
fn the_demo_stack_follows_the_contract() {
    let demo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/stack-demo");
    let read = |relative: &str| fs::read_to_string(demo.join(relative)).unwrap();

    let mut stack_json: Value = serde_json::from_str(&read("stack/stack.json")).unwrap();
    let key = fs::read_to_string(fixtures().join("keys/test-app.pub")).unwrap();
    stack_json["public_keys"] = serde_json::json!([key.trim()]);
    let manifest = StackManifest::parse_source(&serde_json::to_vec(&stack_json).unwrap()).unwrap();
    assert_eq!(manifest.service, "demo");
    assert_eq!(manifest.pre_update.as_deref(), Some("backup"));

    let files: BTreeMap<String, Vec<u8>> = [
        ("stack.json", "stack/stack.json"),
        ("compose.yaml", "stack/compose.yaml"),
        (".env.example", "stack/.env.example"),
        ("files/message.txt", "stack/files/message.txt"),
    ]
    .into_iter()
    .map(|(name, source)| (name.to_owned(), read(source).into_bytes()))
    .collect();
    archive::read(&archive::write(&files).unwrap()).unwrap();

    let env_example = read("stack/.env.example");
    let settings: Vec<&str> = env_example
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name.trim())
        .collect();
    assert_eq!(settings, ["MESSAGE"]);
    let compose = read("stack/compose.yaml");
    let dockerfile = read("app/Dockerfile");
    for name in settings {
        let read_by = format!("${{{name}");
        assert!(compose.contains(&read_by), "compose.yaml ne lit pas {name}");
        assert!(
            dockerfile.contains(&read_by),
            "l'image ne lit pas {name} : le réglage n'aurait aucun effet"
        );
    }
}

fn context_field<'a>(case: &'a Value, defaults: &'a Value, name: &str) -> &'a Value {
    case["context"].get(name).unwrap_or(&defaults[name])
}

/// Analyse le compose d'un cas dans son contexte : celui par défaut du jeu d'essai
/// (`default_context`), où le cas remplace ce qu'il donne.
fn analyze_case(case: &Value, defaults: &Value) -> Result<ComposeReport, StackError> {
    let images: BTreeMap<String, Digest> = context_field(case, defaults, "images")
        .as_object()
        .unwrap()
        .iter()
        .map(|(image, digest)| {
            (
                image.clone(),
                Digest::parse(digest.as_str().unwrap()).unwrap(),
            )
        })
        .collect();
    let context = ComposeContext {
        app: context_field(case, defaults, "app").as_str().unwrap(),
        host: context_field(case, defaults, "host").as_str().unwrap(),
        project_dir: context_field(case, defaults, "project_dir")
            .as_str()
            .unwrap(),
        service: context_field(case, defaults, "service").as_str().unwrap(),
        pre_update: context_field(case, defaults, "pre_update").as_str(),
        images: &images,
    };
    compose::analyze(&body(case), &context)
}

/// Un cas qui attend toutes les permissions du jeu d'essai (`all_permissions`).
fn expects_all_permissions(case: &Value) -> bool {
    case["permissions"] == "@permissions"
}

#[test]
fn compose_cases() {
    let file = read_json("stack/compose.json");
    let defaults = &file["default_context"];
    let expected_all: Vec<String> = file["all_permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    for case in file["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let result = analyze_case(case, defaults);
        assert_eq!(
            outcome(&result),
            case["expected"].as_str().unwrap(),
            "{name} : {result:?}"
        );
        let Ok(report) = result else { continue };
        let expected: Vec<String> = if expects_all_permissions(case) {
            expected_all.clone()
        } else {
            case["permissions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        let keys: Vec<String> = report.permissions.iter().map(Permission::key).collect();
        assert_eq!(keys, expected, "{name}");
        let warnings: Vec<String> = report
            .warnings
            .iter()
            .map(|w| format!("{}|{}|{}", w.service, w.kind, w.value))
            .collect();
        let expected_warnings: Vec<&str> = case["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(warnings, expected_warnings, "{name}");
        if let Some(labels) = case["labels"].as_array() {
            let shown: Vec<String> = report.permissions.iter().map(ToString::to_string).collect();
            let expected: Vec<&str> = labels.iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(shown, expected, "{name}");
        }
        for permission in &report.permissions {
            assert!(
                !permission.label().is_empty() && !permission.to_string().contains('|'),
                "{name}"
            );
        }
    }
}

/// Le `compose.yaml` d'une archive, contrôlé tel qu'il est écrit (`archived` du jeu d'essai).
#[test]
fn archived_compose_cases() {
    let file = read_json("stack/compose.json");
    let cases = file["archived"].as_array().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        // `raw` : le texte ; `file` : un compose réel, gardé tel qu'il a été publié.
        let result = compose::check_archived(&body(case));
        assert_eq!(
            outcome(&result),
            case["expected"].as_str().unwrap(),
            "{name} : {result:?}"
        );
        seen.insert(outcome(&result));
    }
    for code in [
        "ok",
        "compose_not_json",
        "compose_field_unsupported",
        "env_file_outside_files",
    ] {
        assert!(seen.contains(code), "aucun cas ne donne {code}");
    }
    // Ce n'est pas de l'UTF-8 : le jeu d'essai, en JSON, ne sait pas l'écrire.
    assert_eq!(
        outcome(&compose::check_archived(b"{\"services\": {}}\xff")),
        "compose_not_json"
    );
}

/// Les libellés montrés à l'utilisateur (`all_permission_labels`), pour chaque cas du jeu d'essai
/// qui demande toutes les permissions, dans le contexte que donne le jeu d'essai.
#[test]
fn compose_labels() {
    let file = read_json("stack/compose.json");
    let expected: Vec<&str> = file["all_permission_labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let cases: Vec<&Value> = file["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| expects_all_permissions(case))
        .collect();
    assert!(
        !cases.is_empty(),
        "aucun cas ne demande toutes les permissions"
    );
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let report = analyze_case(case, &file["default_context"]).unwrap();
        let shown: Vec<String> = report.permissions.iter().map(ToString::to_string).collect();
        assert_eq!(shown, expected, "{name}");
    }
}

#[test]
fn oci_cases() {
    for case in cases("stack/oci.json") {
        let name = case["name"].as_str().unwrap();
        let bytes = body(&case);
        if let Some(digest) = case["digest"].as_str() {
            assert_eq!(
                Digest::of(&bytes).as_str(),
                digest,
                "{name} : empreinte du fichier"
            );
        }
        let check = &case["check"];
        match case["kind"].as_str().unwrap() {
            "manifest" => {
                let result = oci::parse_manifest(&bytes, case["content_type"].as_str().unwrap());
                assert_eq!(
                    outcome(&result),
                    case["expected"].as_str().unwrap(),
                    "{name} : {result:?}"
                );
                let Ok(object) = result else { continue };
                if let Some(platforms) = check["platforms"].as_array() {
                    let expected: Vec<&str> =
                        platforms.iter().map(|v| v.as_str().unwrap()).collect();
                    assert_eq!(object.platforms(), expected, "{name}");
                }
                if let Some(references) = check["references"].as_array() {
                    let found: Vec<(String, String)> = object
                        .references()
                        .into_iter()
                        .map(|(digest, kind)| (kind.as_str().to_owned(), digest.to_string()))
                        .collect();
                    let expected: Vec<(String, String)> = references
                        .iter()
                        .map(|pair| {
                            (
                                pair[0].as_str().unwrap().to_owned(),
                                pair[1].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect();
                    assert_eq!(found, expected, "{name}");
                }
                if let OciObject::Image { layers, .. } = &object {
                    assert!(
                        layers
                            .iter()
                            .all(|layer| oci::LAYER_TYPES.contains(&layer.media_type.as_str()))
                    );
                }
            }
            "config" => {
                let result = oci::config_arch(&bytes);
                assert_eq!(
                    outcome(&result),
                    case["expected"].as_str().unwrap(),
                    "{name} : {result:?}"
                );
                if let Ok(arch) = result {
                    assert_eq!(arch.as_str(), check["arch"].as_str().unwrap(), "{name}");
                }
            }
            other => panic!("kind {other}"),
        }
    }
}

#[test]
fn control_cases() {
    let file = read_json("stack/control.json");
    for case in file["requests"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let raw = match case["raw"].as_str().unwrap() {
            "@long" => format!(
                "{{\"format\": 1, \"action\": \"check\", \"x\": \"{}\"}}",
                "x".repeat(4200)
            ),
            raw => raw.to_owned(),
        };
        let found = match Request::parse(raw.as_bytes()) {
            Ok(Request::Check) => "check".to_owned(),
            Ok(Request::Update { version }) => format!("update:{version}"),
            Err(error) => error.code().to_owned(),
        };
        assert_eq!(found, case["expected"].as_str().unwrap(), "{name}");
    }
    for case in file["statuses"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let bytes = serde_json::to_vec(&case["document"]).unwrap();
        let parsed = serde_json::from_slice::<Status>(&bytes);
        assert_eq!(
            parsed.is_ok(),
            case["valid"].as_bool().unwrap(),
            "{name} : {parsed:?}"
        );
        if let Ok(status) = parsed {
            let written: Value = serde_json::from_str(&status.to_json()).unwrap();
            assert_eq!(written, case["document"], "{name} : réécriture");
        }
    }
}

/// Un `.env.example` ne porte pas les réglages de Docker ni de Docker Compose, que Compose lirait
/// dans le `.env` de la machine.
#[test]
fn settings_named_for_docker_or_compose_are_refused() {
    use am_protocol::stack::compose::check_env_example;
    for accepted in [
        "",
        "# Port de l'interface\nPORT=8080\n",
        "export TZ=Europe/Paris\nMY_COMPOSE_FILE=x\n# COMPOSE_PROFILES=x\n",
        "docker_host=x\n",
    ] {
        assert!(check_env_example(accepted.as_bytes()).is_ok(), "{accepted}");
    }
    for (refused, name) in [
        (
            "PORT=8080\nCOMPOSE_PROFILES=drakbase-hooks\n",
            "COMPOSE_PROFILES",
        ),
        (
            "  export COMPOSE_IGNORE_ORPHANS=1\n",
            "COMPOSE_IGNORE_ORPHANS",
        ),
        ("DOCKER_HOST=tcp://ailleurs:2375\n", "DOCKER_HOST"),
        // Docker Compose lit aussi `export` suivi d'une tabulation.
        ("export\tCOMPOSE_PROFILES=x\n", "COMPOSE_PROFILES"),
        ("COMPOSE_FILE = x\n", "COMPOSE_FILE"),
    ] {
        let error = check_env_example(refused.as_bytes()).unwrap_err();
        assert_eq!(error.code(), "env_name_reserved", "{refused}");
        assert!(error.to_string().contains(name), "{error}");
    }
}

/// Un `.env.example` ne contient que des lignes que l'agent lit comme Docker Compose : vides,
/// commentaires, ou `NOM=valeur` sur une ligne.
#[test]
fn env_example_lines_the_agent_would_misread_are_refused() {
    use am_protocol::stack::compose::{check_env_example, env_variable, env_variable_as_written};
    for accepted in [
        "PORT=8080\r\nTZ=Europe/Paris\r\n",
        "\t# commentaire\n\n  export\tTZ = Europe/Paris\n",
        "NOM=\"une valeur\" # commentaire\nAUTRE='x'\nVIDE=\n",
        "MOTIF=a\"b\nexport=1\n_X=\"\\\"\"\n",
        "A=\"x\"#commentaire\nB='y' \t# commentaire\nC=x B=2\n",
    ] {
        assert!(check_env_example(accepted.as_bytes()).is_ok(), "{accepted}");
    }
    for (refused, line) in [
        // Formes que Docker Compose lit, mais pas l'agent.
        ("PORT=8080\nTZ: Europe/Paris\n", 2),
        ("HOME\n", 1),
        ("COMPOSE_FILE\n", 1),
        // Valeur qui continue à la ligne suivante.
        ("A=\"x\nB=2\"\n", 1),
        ("A='x\nB=2'\n", 1),
        // Pour Docker Compose, un guillemet après une barre oblique inverse ne referme rien.
        ("A=\"x\\\\\"\nB=2\n", 1),
        // Après un guillemet fermant, Docker Compose lit une seconde variable.
        ("A=\"x\" COMPOSE_PROFILES=secret\n", 1),
        ("PORT=1\nA='x'B=2\n", 2),
        // Ni une ligne, ni un commentaire.
        ("PORT=1\n\nn'importe quoi\n", 3),
        ("=1\n", 1),
        ("1A=2\n", 1),
        ("A-B=1\n", 1),
        // Caractères de contrôle, et octets qui ne sont pas de l'UTF-8.
        ("A=1\rB=2\n", 1),
        ("A=1\u{0085}B=2\n", 1),
        ("A=\u{1b}[2K\n", 1),
    ] {
        let error = check_env_example(refused.as_bytes()).unwrap_err();
        assert_eq!(error.code(), "env_line_unsupported", "{refused:?}");
        assert!(
            error.to_string().contains(&format!("ligne {line} ")),
            "{refused:?} : {error}"
        );
    }
    let error = check_env_example(b"A=\xff\n").unwrap_err();
    assert_eq!(error.code(), "env_line_unsupported");

    assert_eq!(env_variable("PORT=8080"), Some("PORT"));
    assert_eq!(env_variable("  export TZ = Europe/Paris"), Some("TZ"));
    assert_eq!(env_variable("export\tTZ=UTC"), Some("TZ"));
    assert_eq!(env_variable("export=1"), Some("export"));
    assert_eq!(env_variable("_X="), Some("_X"));
    for no in ["# PORT=1", "", "1A=2", "PORT", "A-B=1", "=1", "TZ: UTC"] {
        assert_eq!(env_variable(no), None, "{no}");
    }
    // Dans le .env de la machine, les formes que Docker Compose lit aussi.
    for written in ["TZ: UTC", "TZ:UTC", "TZ : UTC", "export TZ: UTC", "TZ=UTC"] {
        assert_eq!(env_variable_as_written(written), Some("TZ"), "{written}");
    }
    for no in ["# TZ: UTC", "TZ", "Fuseau horaire : UTC", ": UTC"] {
        assert_eq!(env_variable_as_written(no), None, "{no}");
    }
}

/// Ce que `uninstall --delete-data` supprimerait : les volumes que Compose crée pour la stack,
/// ceux à nom fixe étant signalés.
#[test]
fn created_volumes_tell_the_shared_ones() {
    use am_protocol::stack::compose::{CreatedVolume, created_volumes};
    let config = serde_json::json!({
        "services": {},
        "volumes": {
            "data": { "name": "demo_data" },
            "cache": null,
            "photos": { "name": "photos" },
            // `demo_x_y` est aussi le volume `y` d'un projet `demo_x`.
            "x_y": { "name": "demo_x_y" },
            "ailleurs": { "name": "ailleurs", "external": true },
        },
    });
    let volume = |name: &str, shared| CreatedVolume {
        name: name.to_owned(),
        shared,
    };
    assert_eq!(
        created_volumes(&serde_json::to_vec(&config).unwrap(), "demo"),
        [
            volume("demo_cache", false),
            volume("demo_data", false),
            volume("demo_x_y", true),
            volume("photos", true),
        ]
    );
    assert!(created_volumes(b"{}", "demo").is_empty());
    assert!(created_volumes(b"illisible", "demo").is_empty());
}

/// Les chemins de la machine que l'agent résout, et ce que leur texte annonce.
#[test]
fn host_sources_and_what_they_give() {
    use am_protocol::stack::compose::{HostPathKind, HostSource, host_path_kind, host_sources};
    let config = serde_json::json!({
        "services": { "app": { "volumes": [
            { "type": "bind", "source": "/srv//photos", "target": "/a", "read_only": true },
            { "type": "bind", "source": "/srv/photos", "target": "/b" },
            { "type": "bind", "source": "/srv/lecture", "target": "/c", "read_only": true },
            { "type": "bind", "source": "/projet/files/x", "target": "/d" },
            { "type": "volume", "source": "data", "target": "/e" },
        ] } },
        "configs": { "c": { "file": "/srv/config.json" } },
        "volumes": { "data": { "driver_opts": { "device": "/mnt/disque", "o": "bind" } } },
    });
    let source = |path: &str, writable| HostSource {
        path: path.to_owned(),
        writable,
    };
    assert_eq!(
        host_sources(&serde_json::to_vec(&config).unwrap(), "/projet"),
        [
            source("/mnt/disque", true),
            source("/srv/config.json", false),
            source("/srv/lecture", false),
            // Monté deux fois : en écriture une fois suffit.
            source("/srv/photos", true),
        ]
    );
    for (path, kind) in [
        ("/var/lib/drakbase-stack/demo", HostPathKind::AgentDir),
        ("/run/docker.sock", HostPathKind::EngineSocket),
        ("/etc/cron.d", HostPathKind::SystemDir),
        ("//usr", HostPathKind::SystemDir),
        ("/var/lib", HostPathKind::SystemDir),
        ("/srv/photos", HostPathKind::Plain),
        ("/etcetera", HostPathKind::Plain),
    ] {
        assert_eq!(host_path_kind(path), kind, "{path}");
    }
}
