//! Essais de `init` et `doctor` par le binaire, sur des projets d'essai (une app Tauri, une app
//! Swift) et contre le faux serveur de `tests/support/fake_server.rs`. Les clés, leurs mots de
//! passe et les identifiants vont dans le dossier temporaire de chaque essai, les sauvegardes dans
//! une base KeePassXC jetable, et les certificats de signature de code sont simulés : aucun essai
//! ne lit ni n'écrit le vrai trousseau. L'essai Tauri joue l'administration pendant qu'`init`
//! attend.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use am_protocol::Channel;
use am_protocol::signature::PublicKey;
use serde_json::{Value, json};

use support::fake_server::{FakeKey, FakeServer, State};
use support::{
    KEEPASSXC_PASSWORD, both, drakbase, fixture_keys, fixture_keys_saved, keepassxc_database,
    keepassxc_entry, login, public_key, run, run_acting, stdout,
};

/// Le certificat de signature de code que voit l'outil, à la place de ceux du trousseau.
const IDENTITY: &str = "Camille Essai - signature de code";
/// La version des clients que l'outil inscrit dans les projets : la sienne.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const SWIFT_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>fr.example.Demo</string>
	<key>CFBundleShortVersionString</key>
	<string>__VERSION__</string>
</dict>
</plist>
"#;

fn key_id(public_key: &str) -> String {
    PublicKey::from_tauri(public_key.trim())
        .unwrap()
        .key_id()
        .to_hex()
}

/// Le binaire, lancé à la racine du projet.
fn in_project(home: &Path, project: &Path) -> Command {
    let mut command = drakbase(home);
    command
        .current_dir(project)
        .env("APPSMARKET_CODESIGN_IDENTITIES", IDENTITY);
    command
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

/// Une app Tauri comme la crée `npm create tauri-app`, sans rien des mises à jour.
fn tauri_project(dir: &Path) {
    write(
        &dir.join("package.json"),
        r#"{
  "name": "demo",
  "private": true,
  "version": "0.2.0",
  "type": "module",
  "dependencies": {
    "@tauri-apps/api": "^2"
  }
}
"#,
    );
    write(
        &dir.join("src-tauri/tauri.conf.json"),
        r#"{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "Démo",
  "version": "0.2.0",
  "identifier": "fr.example.demo",
  "app": {
    "windows": [{ "title": "Démo" }]
  },
  "bundle": {
    "active": true,
    "targets": "all"
  }
}
"#,
    );
    write(
        &dir.join("src-tauri/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.2.0\"\nedition = \"2021\"\n\n[dependencies]\ntauri = { version = \"2\", features = [] }\n",
    );
    write(
        &dir.join("src-tauri/capabilities/default.json"),
        r#"{
  "identifier": "default",
  "windows": ["main"],
  "permissions": ["core:default"]
}
"#,
    );
    write(
        &dir.join("src-tauri/src/main.rs"),
        "fn main() {\n    tauri::Builder::default()\n        .run(tauri::generate_context!())\n        .unwrap();\n}\n",
    );
    write(&dir.join("src/main.tsx"), "console.log(\"Démo\");\n");
}

/// Une app Swift en paquet SwiftPM, avec son Info.plist et son fichier VERSION, comme Kiosque.
fn swift_project(dir: &Path) {
    write(
        &dir.join("Package.swift"),
        "// swift-tools-version: 6.2\nimport PackageDescription\n\nlet package = Package(\n    name: \"Demo\",\n    targets: [.executableTarget(name: \"Demo\")]\n)\n",
    );
    write(&dir.join("scripts/Info.plist"), SWIFT_PLIST);
    write(&dir.join("VERSION"), "1.2.0\n");
    write(
        &dir.join("Sources/Demo/DemoApp.swift"),
        "import SwiftUI\n\n@main\nstruct DemoApp: App {\n    var body: some Scene { WindowGroup { Text(\"Démo\") } }\n}\n",
    );
}

fn plist_string(info: &plist::Dictionary, key: &str) -> Option<String> {
    info.get(key)
        .and_then(plist::Value::as_string)
        .map(str::to_owned)
}

#[test]
fn a_tauri_app_is_set_up_then_checked() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    let Some(database) = keepassxc_database(home.path()) else {
        return;
    };
    tauri_project(dir);
    let mut state = State::demo(Some("fr.example.demo"));
    let app = state.apps.get_mut("test-app").unwrap();
    app.kind = "tauri".to_owned();
    app.keys.clear();
    let server = FakeServer::start(state);
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));

    // Première fois : les clés sont créées et sauvegardées, toute la configuration écrite, puis
    // l'app déclarée. L'outil ouvre l'administration et attend : ici, l'essai active la clé.
    let keys = home.path().join("outil/keys");
    let output = run_acting(
        in_project(home.path(), dir)
            .args([
                "init",
                "test-app",
                "--password-stdin",
                "--sign-id",
                IDENTITY,
                "--keepassxc",
            ])
            .arg(&database),
        &format!("{KEEPASSXC_PASSWORD}\n"),
        || {
            server.state().requests.iter().any(|request| {
                request.method == "GET" && request.path == "/api/v1/publish/apps/test-app"
            })
        },
        || {
            let public = read(&keys.join("test-app.key.pub"));
            server
                .state()
                .apps
                .get_mut("test-app")
                .unwrap()
                .keys
                .push(FakeKey {
                    public_key: public.trim().to_owned(),
                    status: "active".to_owned(),
                });
        },
    );
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    let main_public = read(&keys.join("test-app.key.pub"));
    let main_id = key_id(&main_public);
    assert!(!keys.join("test-app.backup.key").exists(), "{text}");
    assert!(keys.join("test-app.backup.key.pub").is_file(), "{text}");
    for expected in [
        "App Tauri : src-tauri/tauri.conf.json",
        "Aucune clé pour l'app test-app : création",
        "« Drakbase - Apps Market/test-app - clé principale » et « Drakbase - Apps Market/test-app - clé de secours » écrites et relues",
        "✓ src-tauri/tauri.conf.json : clé publique, adresse de vérification",
        "✓ src-tauri/capabilities/default.json : updater:default, process:allow-restart, store:default",
        "✓ src-tauri/Cargo.toml : tauri-plugin-updater, tauri-plugin-process, tauri-plugin-store, tauri-plugin-os",
        "✓ package.json : @drakbase/tauri, @tauri-apps/plugin-updater",
        &format!(
            "{}/admin/add-app#slug=test-app&name=D%C3%A9mo&kind=tauri&bundle_id=fr.example.demo&public_key=",
            server.url
        ),
        &format!("✓ app « Test App » déclarée sur le serveur, clé {main_id} active"),
        "Reste à faire :",
        ".plugin(tauri_plugin_updater::Builder::new().build())",
        "drakbase exec test-app -- npx tauri build",
        "Contrôle final, comme « drakbase doctor » :",
        "! le plugin updater n'est pas enregistré dans src-tauri/src",
        "Prêt, avec 4 avertissement(s).",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("Déclarer l'app sur le serveur"), "{text}");
    // KeePassXC garde les deux clés ; le mot de passe de la principale est aussi rangé ici.
    let saved = keepassxc_entry(
        &database,
        "Drakbase - Apps Market/test-app - clé principale",
    )
    .unwrap();
    assert_eq!(saved.username, main_id);
    let stored = read_json(&home.path().join("outil/key-passwords.json"));
    assert_eq!(stored[&main_id], json!(saved.password));
    assert!(
        keepassxc_entry(
            &database,
            "Drakbase - Apps Market/test-app - clé de secours"
        )
        .is_some()
    );

    let config_text = read(&dir.join("src-tauri/tauri.conf.json"));
    let config: Value = serde_json::from_str(&config_text).unwrap();
    let endpoint = format!(
        "{}/api/v1/update/test-app/{{{{target}}}}/{{{{arch}}}}/{{{{current_version}}}}?bundle_type={{{{bundle_type}}}}",
        server.url
    );
    assert_eq!(
        config["plugins"]["updater"],
        json!({
            "pubkey": main_public.trim(),
            "endpoints": [endpoint],
            "requireSignedVersion": true,
            "windows": {"installMode": "passive"},
        })
    );
    assert_eq!(config["bundle"]["createUpdaterArtifacts"], json!(true));
    assert_eq!(
        config["bundle"]["windows"]["nsis"]["installMode"],
        json!("currentUser")
    );
    assert_eq!(
        config["bundle"]["macOS"]["signingIdentity"],
        json!(IDENTITY)
    );
    // L'ordre des clés du développeur est gardé ; les nouvelles viennent après.
    let position = |key: &str| config_text.find(&format!("\"{key}\"")).unwrap();
    assert!(
        position("$schema") < position("productName"),
        "{config_text}"
    );
    assert!(position("identifier") < position("app"), "{config_text}");
    assert!(position("bundle") < position("plugins"), "{config_text}");
    assert!(config_text.ends_with("}\n"), "{config_text}");

    assert_eq!(
        read_json(&dir.join("src-tauri/capabilities/default.json"))["permissions"],
        json!([
            "core:default",
            "updater:default",
            "process:allow-restart",
            "store:default"
        ])
    );
    let cargo = read(&dir.join("src-tauri/Cargo.toml"));
    for line in [
        "tauri-plugin-updater = \"=2.14.0\"",
        "tauri-plugin-process = \"=2.4.0\"",
        "tauri-plugin-store = \"=2.5.0\"",
        "tauri-plugin-os = \"=2.4.0\"",
        "tauri = { version = \"2\", features = [] }",
    ] {
        assert!(cargo.contains(line), "{line}\n{cargo}");
    }
    let package = read_json(&dir.join("package.json"));
    assert_eq!(
        package["dependencies"],
        json!({
            "@tauri-apps/api": "^2",
            "@drakbase/tauri": format!("git+https://github.com/DrakBase/client.git#semver:^{CLIENT_VERSION}"),
            "@tauri-apps/plugin-updater": "2.14.0",
            "@tauri-apps/plugin-process": "2.4.0",
            "@tauri-apps/plugin-store": "2.5.0",
            "@tauri-apps/plugin-os": "2.4.0",
        })
    );

    // Une seconde fois : tout est déjà en place, et aucun fichier ne change.
    let files = [
        "src-tauri/tauri.conf.json",
        "src-tauri/capabilities/default.json",
        "src-tauri/Cargo.toml",
        "package.json",
    ];
    let before: Vec<String> = files.iter().map(|file| read(&dir.join(file))).collect();
    let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    assert_eq!(text.matches("déjà en place").count(), 4, "{text}");
    assert!(!text.contains("création"), "{text}");
    for expected in [
        "Sauvegarde : KeePassXC",
        &format!("✓ app « Test App » sur le serveur, clé {main_id} active"),
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let after: Vec<String> = files.iter().map(|file| read(&dir.join(file))).collect();
    assert_eq!(before, after);

    // Il reste le code : doctor le dit, sans erreur.
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ app Tauri : src-tauri/tauri.conf.json",
        "✓ app : test-app",
        "✓ mot de passe de la clé principale dans le fichier",
        "✓ clés sauvegardées dans KeePassXC",
        &format!("✓ clé {main_id} active sur le serveur"),
        "! le plugin os (version du système) n'est pas enregistré dans src-tauri/src",
        "! le plugin updater n'est pas enregistré dans src-tauri/src",
        "! l'interface n'utilise pas encore @drakbase/tauri",
        "Prêt, avec 4 avertissement(s).",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    // Plugins enregistrés, aide utilisée : tout est prêt.
    write(
        &dir.join("src-tauri/src/main.rs"),
        "fn main() {\n    tauri::Builder::default()\n        .plugin(tauri_plugin_updater::Builder::new().build())\n        .plugin(tauri_plugin_process::init())\n        .plugin(tauri_plugin_os::init())\n        .run(tauri::generate_context!())\n        .unwrap();\n}\n",
    );
    write(
        &dir.join("src/main.tsx"),
        "import { useUpdater } from \"@drakbase/tauri/react\";\n",
    );
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        &format!("✓ clé {main_id} active sur le serveur"),
        &format!("✓ certificat « {IDENTITY} » présent"),
        "✓ version du projet : 0.2.0",
        "Tout est prêt.",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("! ") && !text.contains("✗"), "{text}");

    // Version déjà publiée et script piégé par la locale : deux avertissements. Certificat absent
    // du trousseau : un problème.
    server
        .state()
        .publish_version("test-app", "0.2.0", Channel::Stable);
    write(
        &dir.join("scripts/ship.sh"),
        "#!/bin/bash\nAPP=demo\necho \"▸ Publication de $APP…\"\n",
    );
    let output = run(in_project(home.path(), dir)
        .env("APPSMARKET_CODESIGN_IDENTITIES", "Autre certificat")
        .arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "! la version du projet, 0.2.0, n'est pas plus haute que la dernière version stable publiée, 0.2.0",
        "! scripts/ship.sh:3 : $APP est collé à un caractère non ASCII",
        "écris ${APP}",
        &format!("✗ le certificat « {IDENTITY} » de tauri.conf.json n'est pas dans le trousseau"),
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn a_swift_app_is_set_up_then_checked() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    swift_project(dir);
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));

    let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "App Swift : Package.swift, Info.plist : scripts/Info.plist",
        &format!(
            "principale {}, secours {}",
            key_id(&public_key("test-app")),
            key_id(&public_key("other-app"))
        ),
        "✓ scripts/Info.plist : adresse de vérification (DrakBaseFeedURL), clés publiques",
        &format!(
            ".package(url: \"https://github.com/DrakBase/client.git\", from: \"{CLIENT_VERSION}\"),"
        ),
        ".product(name: \"DrakBase\", package: \"client\")",
        "RelaunchGate.waitForPreviousInstance()",
        "Sauvegarde : KeePassXC (/essai/base.kdbx), vérifiée le 2026-10-28",
        &format!(
            "✓ app « Test App » sur le serveur, clé {} active",
            key_id(&public_key("test-app"))
        ),
        "drakbase release <Nom.app> --app test-app",
        "Prêt, avec 3 avertissement(s).",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("création"), "{text}");
    let plist_path = dir.join("scripts/Info.plist");
    let info = plist::Value::from_file(&plist_path)
        .unwrap()
        .into_dictionary()
        .unwrap();
    assert_eq!(
        plist_string(&info, "DrakBaseFeedURL"),
        Some(format!("{}/api/v1/update/test-app", server.url))
    );
    let keys: Vec<&str> = info
        .get("DrakBasePublicKeys")
        .and_then(plist::Value::as_array)
        .unwrap()
        .iter()
        .map(|key| key.as_string().unwrap())
        .collect();
    assert_eq!(keys, [public_key("test-app"), public_key("other-app")]);
    assert_eq!(
        plist_string(&info, "CFBundleIdentifier").as_deref(),
        Some("fr.example.Demo")
    );
    assert_eq!(
        plist_string(&info, "CFBundleShortVersionString").as_deref(),
        Some("__VERSION__")
    );

    let before = read(&plist_path);
    let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("· scripts/Info.plist : déjà en place"),
        "{}",
        both(&output)
    );
    assert_eq!(read(&plist_path), before);

    // Configuration et serveur en ordre ; il manque la dépendance et le code.
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ app : test-app",
        &format!(
            "✓ adresse de vérification (DrakBaseFeedURL) : {}/api/v1/update/test-app",
            server.url
        ),
        "✓ identifiant de bundle : fr.example.Demo",
        "✓ version du projet : 1.2.0",
        "! dépendance DrakBase absente du projet",
        "! introuvable dans le code : le point d'entrée attend l'ancienne instance",
        "! introuvable dans le code : l'app crée son Updater",
        "Prêt, avec 3 avertissement(s).",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    write(
        &dir.join("Package.swift"),
        "// swift-tools-version: 6.2\nimport PackageDescription\n\nlet package = Package(\n    name: \"Demo\",\n    dependencies: [.package(url: \"https://github.com/DrakBase/client.git\", from: \"0.1.0\")],\n    targets: [.executableTarget(name: \"Demo\", dependencies: [.product(name: \"DrakBase\", package: \"client\")])]\n)\n",
    );
    write(
        &dir.join("Sources/Demo/DemoApp.swift"),
        "import DrakBase\nimport SwiftUI\n\n@main @MainActor enum Main {\n    static func main() { RelaunchGate.waitForPreviousInstance(); DemoApp.main() }\n}\n\nstruct DemoApp: App {\n    @State private var updater = Updater()\n    var body: some Scene { WindowGroup { Text(\"Démo\") } }\n}\n",
    );
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("Tout est prêt."),
        "{}",
        both(&output)
    );

    // L'ancien nom du produit, AppsMarketKit, est reconnu mais signalé.
    let package = read(&dir.join("Package.swift"));
    write(
        &dir.join("Package.swift"),
        &package.replace("name: \"DrakBase\"", "name: \"AppsMarketKit\""),
    );
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output)
            .contains("! dépendance AppsMarketKit : le paquet s'appelle désormais DrakBase"),
        "{}",
        both(&output)
    );

    // Les anciens dépôts, privés : par leur adresse et leur nom dans `package:`, ou par leur seul
    // nom quand l'adresse a déjà changé. Signalés, avec les deux à écrire.
    let public_name = "package: \"client\"";
    assert!(package.contains("https://github.com/DrakBase/client.git"));
    assert!(package.contains(public_name));
    let at = |address: &str, name: &str| {
        package
            .replace("https://github.com/DrakBase/client.git", address)
            .replace(public_name, &format!("package: \"{name}\""))
    };
    for (manifest, old) in [
        (
            at(
                "git@github.com:kcampion/drakbase-apps-market.git",
                "drakbase-apps-market",
            ),
            "drakbase-apps-market",
        ),
        (
            at("git@github.com:DrakBase/apps-market.git", "apps-market"),
            "apps-market",
        ),
        (
            at("https://github.com/DrakBase/client.git", "apps-market"),
            "apps-market",
        ),
    ] {
        write(&dir.join("Package.swift"), &manifest);
        let output = run(in_project(home.path(), dir).arg("doctor"));
        assert!(output.status.success(), "{}", both(&output));
        let text = stdout(&output);
        for expected in [
            format!("! paquet DrakBase pris à l'ancien dépôt ({old})"),
            "  → dans Package.swift : .package(url: \"https://github.com/DrakBase/client.git\", …) et .product(name: \"DrakBase\", package: \"client\")".to_owned(),
        ] {
            assert!(text.contains(&expected), "{expected}\n{text}");
        }
        let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
        assert!(output.status.success(), "{}", both(&output));
        let text = stdout(&output);
        for expected in [
            "Passer le paquet au dépôt public, dans Package.swift : son adresse, et son nom dans .product(…) :",
            ".package(url: \"https://github.com/DrakBase/client.git\"",
            ".product(name: \"DrakBase\", package: \"client\")",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
    }
    write(&dir.join("Package.swift"), &package);

    // Sans certificat de signature de code, une app Swift ne recevrait aucune mise à jour.
    let output = run(in_project(home.path(), dir)
        .env("APPSMARKET_CODESIGN_IDENTITIES", "")
        .arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stdout(&output).contains("✗ aucun certificat de signature de code dans le trousseau"),
        "{}",
        both(&output)
    );

    // Un bundle construit sans signature est refusé.
    let bundle = dir.join("build/Demo.app");
    write(&bundle.join("Contents/Info.plist"), SWIFT_PLIST);
    write(&bundle.join("Contents/MacOS/Demo"), "#!/bin/sh\n");
    let output = run(in_project(home.path(), dir).args(["doctor", "--bundle", "build/Demo.app"]));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    assert!(text.contains("n'est pas signé"), "{text}");
    assert!(
        text.contains("✗ adresse de vérification (DrakBaseFeedURL) : absente"),
        "{text}"
    );
}

/// Les fichiers de plateforme de Tauri (`tauri.macos.conf.json`…) : `init` et `doctor` lisent,
/// pour chaque OS, ce que lira `tauri build`, comme le fait `publish`.
#[test]
fn tauri_platform_files_are_read_like_tauri_build() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    tauri_project(dir);
    let mut state = State::demo(Some("fr.example.demo"));
    state.apps.get_mut("test-app").unwrap().kind = "tauri".to_owned();
    let server = FakeServer::start(state);
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
    let config = dir.join("src-tauri/tauri.conf.json");
    let main_id = key_id(&public_key("test-app"));
    let other_id = key_id(&public_key("other-app"));

    // Le certificat des builds macOS est dans le fichier de la plateforme, que lit tauri build :
    // --sign-id ne l'écrit pas dans tauri.conf.json, où il resterait sans effet, et le dit.
    let macos = dir.join("src-tauri/tauri.macos.conf.json");
    let other_certificate =
        r#"{ "bundle": { "macOS": { "signingIdentity": "Autre certificat" } } }"#;
    write(&macos, other_certificate);
    let output =
        run(in_project(home.path(), dir).args(["init", "test-app", "--sign-id", IDENTITY]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✓ src-tauri/tauri.conf.json : clé publique, adresse de vérification".to_owned(),
        format!(
            "! src-tauri/tauri.macos.conf.json donne déjà bundle.macOS.signingIdentity pour les builds macOS (« Autre certificat ») : « {IDENTITY} » n'est pas écrit dans src-tauri/tauri.conf.json"
        ),
        // Le contrôle final lit le certificat qui servira.
        "✗ le certificat « Autre certificat » de tauri.macos.conf.json n'est pas dans le trousseau"
            .to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    assert!(!text.contains("certificat macOS «"), "{text}");
    assert!(
        read_json(&config)["bundle"].get("macOS").is_none(),
        "{}",
        read(&config)
    );
    assert_eq!(read(&macos), other_certificate);
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert!(
        stdout(&output).contains(
            "✗ le certificat « Autre certificat » de tauri.macos.conf.json n'est pas dans le trousseau"
        ),
        "{}",
        both(&output)
    );

    // Le bon certificat y est déjà : rien à écrire, rien à signaler.
    write(
        &macos,
        &json!({ "bundle": { "macOS": { "signingIdentity": IDENTITY } } }).to_string(),
    );
    let before = read(&config);
    let output =
        run(in_project(home.path(), dir).args(["init", "test-app", "--sign-id", IDENTITY]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    assert!(
        text.contains("· src-tauri/tauri.conf.json : déjà en place"),
        "{text}"
    );
    assert!(!text.contains("donne déjà"), "{text}");
    assert_eq!(read(&config), before);
    let output = run(in_project(home.path(), dir).arg("doctor"));
    let text = stdout(&output);
    for expected in [
        format!("✓ certificat « {IDENTITY} » présent"),
        // Aucun fichier de plateforme ne change la clé : une seule ligne, pour tous les OS.
        format!("✓ clé embarquée (plugins.updater.pubkey) : {main_id}, la clé principale"),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }

    // Une autre clé pour les builds Windows : doctor le dit pour Windows seulement, et init le
    // signale sans toucher au fichier.
    let windows = dir.join("src-tauri/tauri.windows.conf.json");
    let other_key =
        json!({ "plugins": { "updater": { "pubkey": public_key("other-app") } } }).to_string();
    write(&windows, &other_key);
    let output = run(in_project(home.path(), dir).arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        format!(
            "✓ clé embarquée (plugins.updater.pubkey) pour macOS et Linux (src-tauri/tauri.conf.json) : {main_id}, la clé principale"
        ),
        format!(
            "✗ clé embarquée (plugins.updater.pubkey) pour Windows (src-tauri/tauri.windows.conf.json) : {other_id}, alors que la clé principale de l'app est {main_id}"
        ),
        // Les réglages que ce fichier ne change pas restent dits une fois.
        "✓ version signée exigée (requireSignedVersion)\n".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    let expected = format!(
        "! src-tauri/tauri.windows.conf.json change « clé publique » pour les builds Windows (plugins.updater.pubkey y vaut la clé {other_id})"
    );
    assert!(
        stdout(&output).contains(&expected),
        "{expected}\n{}",
        both(&output)
    );
    assert_eq!(read(&windows), other_key);
}

/// `doctor` juge la version du projet comme `publish` : une pré-publication face à la dernière
/// bêta, une autre version face à la dernière stable.
#[test]
fn doctor_compares_the_project_version_with_its_own_channel() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    swift_project(dir);
    let server = FakeServer::start(State::demo(Some("fr.example.Demo")));
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
    let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
    assert!(output.status.success(), "{}", both(&output));
    server
        .state()
        .publish_version("test-app", "1.1.0", Channel::Stable);
    server
        .state()
        .publish_version("test-app", "1.3.0-beta.2", Channel::Beta);
    let doctor = |version: &str| {
        write(&dir.join("VERSION"), version);
        stdout(&run(in_project(home.path(), dir).arg("doctor")))
    };

    // Un correctif de la stable, plus bas que la bêta en cours : publish l'accepte en stable.
    let text = doctor("1.2.0\n");
    assert!(text.contains("✓ version du projet : 1.2.0"), "{text}");
    assert!(!text.contains("l'outil refusera"), "{text}");

    // Pas plus haute que la dernière stable : publish la refuserait.
    let text = doctor("1.1.0\n");
    for expected in [
        "! la version du projet, 1.1.0, n'est pas plus haute que la dernière version stable publiée, 1.1.0",
        "sinon l'outil refusera",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    // Une pré-publication se compare à la dernière bêta.
    let text = doctor("1.3.0-beta.2\n");
    assert!(
        text.contains(
            "! la version du projet, 1.3.0-beta.2, n'est pas plus haute que la dernière version bêta publiée, 1.3.0-beta.2"
        ),
        "{text}"
    );
    let text = doctor("1.3.0-beta.3\n");
    assert!(
        text.contains("✓ version du projet : 1.3.0-beta.3"),
        "{text}"
    );

    // Une bêta plus basse que la dernière stable se publierait, sans que personne la reçoive :
    // un avertissement, mais pas « l'outil refusera ».
    server
        .state()
        .publish_version("test-app", "2.0.0", Channel::Stable);
    let text = doctor("1.4.0-beta.1\n");
    assert!(
        text.contains(
            "! la version du projet, 1.4.0-beta.1, est une pré-publication plus basse que la dernière version stable, 2.0.0"
        ),
        "{text}"
    );
    assert!(!text.contains("l'outil refusera"), "{text}");

    // Déjà publiée sur l'autre canal : c'est son canal qu'il faut changer.
    server
        .state()
        .publish_version("test-app", "2.1.0", Channel::Beta);
    let text = doctor("2.1.0\n");
    for expected in [
        "! la version du projet, 2.1.0, est déjà publiée en bêta",
        "change son canal dans l'administration",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn keys_already_embedded_are_never_replaced() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    let server = "https://maj.example.fr";

    let swift = tempfile::tempdir().unwrap();
    swift_project(swift.path());
    let with_other_key = SWIFT_PLIST.replace(
        "</dict>",
        &format!(
            "\t<key>AppsMarketPublicKeys</key>\n\t<array>\n\t\t<string>{}</string>\n\t</array>\n</dict>",
            public_key("other-app")
        ),
    );
    write(&swift.path().join("scripts/Info.plist"), &with_other_key);
    let output =
        run(in_project(home.path(), swift.path()).args(["init", "test-app", "--server", server]));
    assert!(!output.status.success(), "{}", both(&output));
    assert!(
        both(&output).contains(&format!(
            "embarque déjà d'autres clés (DrakBasePublicKeys, d'abord {})",
            key_id(&public_key("other-app"))
        )),
        "{}",
        both(&output)
    );
    assert_eq!(
        read(&swift.path().join("scripts/Info.plist")),
        with_other_key
    );

    let tauri = tempfile::tempdir().unwrap();
    tauri_project(tauri.path());
    let config = tauri.path().join("src-tauri/tauri.conf.json");
    let mut json = read_json(&config);
    json["plugins"] = json!({"updater": {"pubkey": public_key("other-app")}});
    let with_other_key = serde_json::to_string_pretty(&json).unwrap();
    fs::write(&config, &with_other_key).unwrap();
    let output =
        run(in_project(home.path(), tauri.path()).args(["init", "test-app", "--server", server]));
    assert!(!output.status.success(), "{}", both(&output));
    assert!(
        both(&output).contains("embarque déjà une autre clé publique"),
        "{}",
        both(&output)
    );
    assert_eq!(read(&config), with_other_key);
}

#[test]
fn legacy_info_keys_are_renamed() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let project = tempfile::tempdir().unwrap();
    swift_project(project.path());
    // Une app configurée avant la 0.3 : les mêmes valeurs, sous les anciens noms.
    let keys = [public_key("test-app"), public_key("other-app")]
        .map(|key| format!("\t\t<string>{key}</string>\n"))
        .concat();
    write(
        &project.path().join("scripts/Info.plist"),
        &SWIFT_PLIST.replace(
            "</dict>",
            &format!(
                "\t<key>AppsMarketFeedURL</key>\n\t<string>https://maj.example.fr/api/v1/update/test-app</string>\n\t<key>AppsMarketPublicKeys</key>\n\t<array>\n{keys}\t</array>\n</dict>"
            ),
        ),
    );

    // doctor lit encore les anciennes clés, et les signale.
    let output = run(in_project(home.path(), project.path()).arg("doctor"));
    let text = stdout(&output);
    for expected in [
        "! AppsMarketFeedURL : ancien nom de DrakBaseFeedURL",
        "! AppsMarketPublicKeys : ancien nom de DrakBasePublicKeys",
        "✓ adresse de vérification (DrakBaseFeedURL) : https://maj.example.fr/api/v1/update/test-app",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    let output = run(in_project(home.path(), project.path()).args([
        "init",
        "test-app",
        "--server",
        "https://maj.example.fr",
    ]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    assert!(
        text.contains("AppsMarketFeedURL renommée DrakBaseFeedURL, AppsMarketPublicKeys renommée DrakBasePublicKeys"),
        "{text}"
    );
    let info = plist::Value::from_file(project.path().join("scripts/Info.plist"))
        .unwrap()
        .into_dictionary()
        .unwrap();
    assert_eq!(
        plist_string(&info, "DrakBaseFeedURL").as_deref(),
        Some("https://maj.example.fr/api/v1/update/test-app")
    );
    let embedded: Vec<String> = info
        .get("DrakBasePublicKeys")
        .and_then(plist::Value::as_array)
        .unwrap()
        .iter()
        .filter_map(plist::Value::as_string)
        .map(str::to_owned)
        .collect();
    assert_eq!(embedded, [public_key("test-app"), public_key("other-app")]);
    assert!(!info.contains_key("AppsMarketFeedURL"));
    assert!(!info.contains_key("AppsMarketPublicKeys"));
}

#[test]
fn init_needs_a_server_and_doctor_says_what_is_missing() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let project = tempfile::tempdir().unwrap();
    swift_project(project.path());

    // Sans connexion enregistrée, il faut l'adresse du serveur.
    let output = run(in_project(home.path(), project.path()).args(["init", "test-app"]));
    assert!(!output.status.success(), "{}", both(&output));
    assert!(
        both(&output).contains("drakbase login --server"),
        "{}",
        both(&output)
    );
    let output = run(in_project(home.path(), project.path()).args([
        "init",
        "test-app",
        "--server",
        "https://maj.example.fr/",
    ]));
    assert!(output.status.success(), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "· l'outil n'est pas connecté à https://maj.example.fr",
        "Déclarer l'app sur le serveur : connecte l'outil (« drakbase login --server https://maj.example.fr »)",
        "✗ aucune connexion enregistrée",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    let info = plist::Value::from_file(project.path().join("scripts/Info.plist"))
        .unwrap()
        .into_dictionary()
        .unwrap();
    assert_eq!(
        plist_string(&info, "DrakBaseFeedURL").as_deref(),
        Some("https://maj.example.fr/api/v1/update/test-app")
    );

    // doctor contrôle quand même le projet, sans le serveur.
    let output = run(in_project(home.path(), project.path()).arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    for expected in [
        "✗ aucune connexion enregistrée",
        "✓ app : test-app",
        "✓ adresse de vérification (DrakBaseFeedURL) : https://maj.example.fr/api/v1/update/test-app (serveur non vérifié, faute de connexion)",
        "1 problème(s) à régler",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("Serveur\n"), "{text}");

    // Hors d'un projet, et sans app nommée, il n'y a rien à contrôler.
    let empty = tempfile::tempdir().unwrap();
    let output = run(in_project(home.path(), empty.path()).arg("doctor"));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = stdout(&output);
    assert!(text.contains("! aucun projet d'app"), "{text}");
    assert!(
        text.contains("✗ impossible de savoir quelle app contrôler"),
        "{text}"
    );
}

/// Sans mot de passe rangé pour la clé principale, `doctor` dit où il manque.
#[test]
fn doctor_says_where_the_password_of_the_main_key_is_missing() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    let project = tempfile::tempdir().unwrap();
    swift_project(project.path());

    let output = run(in_project(home.path(), project.path()).args(["doctor", "test-app"]));
    let text = stdout(&output);
    for expected in [
        "! mot de passe de la clé principale absent du fichier ",
        "key-passwords.json : chaque signature le demandera",
        "  → drakbase backup test-app l'y range, après l'avoir vérifié",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

/// L'aide `@drakbase/tauri` s'installe par l'adresse du dépôt, avec une plage de versions. Une
/// app branchée par un outil plus ancien garde une plage qui exclut la version de celui-ci :
/// `doctor` le dit, et `init` la monte. Déclarée autrement, la dépendance n'est jamais touchée.
#[test]
fn a_helper_range_that_excludes_the_tool_is_raised() {
    let home = tempfile::tempdir().unwrap();
    fixture_keys(home.path());
    fixture_keys_saved(home.path());
    let project = tempfile::tempdir().unwrap();
    let dir = project.path();
    tauri_project(dir);
    let mut state = State::demo(Some("fr.example.demo"));
    state.apps.get_mut("test-app").unwrap().kind = "tauri".to_owned();
    let server = FakeServer::start(state);
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
    let package = dir.join("package.json");
    let repository = "git+https://github.com/DrakBase/client.git";
    // La dépendance telle que l'app la déclare, dans cette section de package.json.
    let declare = |section: &str, value: &str| {
        let mut json = read_json(&package);
        json[section]["@drakbase/tauri"] = json!(value);
        write(&package, &serde_json::to_string_pretty(&json).unwrap());
    };
    let init = || {
        let output = run(in_project(home.path(), dir).args(["init", "test-app"]));
        assert!(output.status.success(), "{}", both(&output));
        stdout(&output)
    };
    let doctor = || stdout(&run(in_project(home.path(), dir).arg("doctor")));
    let untouched = "✓ @drakbase/tauri dans package.json";

    // L'app, branchée par un outil plus ancien.
    declare("devDependencies", &format!("{repository}#semver:^0.0.1"));
    init();
    declare("devDependencies", &format!("{repository}#semver:^0.0.1"));
    let text = doctor();
    for expected in [
        format!(
            "! @drakbase/tauri plus ancien que l'outil : la plage ^0.0.1 de package.json s'arrête avant la version {CLIENT_VERSION}"
        ),
        "  → drakbase init test-app monte la plage, puis npm install".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }

    // init la monte là où elle est déclarée, et le contrôle final ne dit plus rien d'elle.
    let text = init();
    let expected = format!("✓ package.json : @drakbase/tauri monté de ^0.0.1 à ^{CLIENT_VERSION}");
    assert!(text.contains(&expected), "{expected}\n{text}");
    assert!(!text.contains("! @drakbase/tauri"), "{text}");
    let json = read_json(&package);
    assert_eq!(
        json["devDependencies"]["@drakbase/tauri"],
        json!(format!("{repository}#semver:^{CLIENT_VERSION}"))
    );
    assert!(json["dependencies"].get("@drakbase/tauri").is_none());
    assert!(doctor().contains(untouched));
    // Une seconde fois, il n'y a plus rien à changer.
    assert!(init().contains("· package.json : déjà en place"));

    // Les anciens dépôts, privés : doctor les signale, et init passe l'aide au dépôt public, par
    // HTTPS, dans la même section, avec la plage telle quelle, ou montée si elle s'arrêtait avant
    // l'outil.
    for (legacy, old) in [
        (
            "git+ssh://git@github.com/kcampion/drakbase-apps-market.git",
            "kcampion/drakbase-apps-market",
        ),
        (
            "git+ssh://git@github.com/DrakBase/apps-market.git",
            "DrakBase/apps-market",
        ),
    ] {
        for (range, said) in [
            (
                format!("^{CLIENT_VERSION}"),
                "@drakbase/tauri pris au dépôt public (DrakBase/client)".to_owned(),
            ),
            (
                "^0.0.1".to_owned(),
                format!(
                    "@drakbase/tauri monté de ^0.0.1 à ^{CLIENT_VERSION}, pris au dépôt public (DrakBase/client)"
                ),
            ),
        ] {
            declare("devDependencies", &format!("{legacy}#semver:{range}"));
            let text = doctor();
            for expected in [
                format!("! @drakbase/tauri pris à l'ancien dépôt ({old})"),
                "  → drakbase init test-app passe au dépôt public DrakBase/client, puis npm install"
                    .to_owned(),
            ] {
                assert!(text.contains(&expected), "{expected}\n{text}");
            }
            let text = init();
            let expected = format!("✓ package.json : {said}");
            assert!(text.contains(&expected), "{expected}\n{text}");
            assert!(!text.contains("ancien dépôt"), "{text}");
            assert_eq!(
                read_json(&package)["devDependencies"]["@drakbase/tauri"],
                json!(format!("{repository}#semver:^{CLIENT_VERSION}"))
            );
            assert!(doctor().contains(untouched));
        }
    }
    // Prise par une étiquette, elle n'a pas de plage : init n'y touche pas, doctor dit quoi écrire.
    declare(
        "devDependencies",
        "git+ssh://git@github.com/kcampion/drakbase-apps-market.git#v0.0.1",
    );
    let before = read(&package);
    let text = init();
    assert!(text.contains("· package.json : déjà en place"), "{text}");
    assert_eq!(read(&package), before);
    let text = doctor();
    for expected in [
        "! @drakbase/tauri pris à l'ancien dépôt (kcampion/drakbase-apps-market)".to_owned(),
        format!(
            "  → écris {repository}#semver:^{CLIENT_VERSION} dans package.json, puis npm install"
        ),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    assert!(!text.contains(untouched), "{text}");

    // Une plage illisible n'est pas réécrite : init et doctor la signalent, et disent quoi faire.
    declare(
        "devDependencies",
        &format!("{repository}#semver:la dernière"),
    );
    let before = read(&package);
    for text in [doctor(), init()] {
        for expected in [
            "! @drakbase/tauri : plage « la dernière » illisible dans package.json".to_owned(),
            format!(
                "  → drakbase init n'y touche pas : vérifie qu'elle admet la version {CLIENT_VERSION} de l'outil, ou écris ^{CLIENT_VERSION} à sa place, puis npm install"
            ),
        ] {
            assert!(text.contains(&expected), "{expected}\n{text}");
        }
        assert!(!text.contains("remis à"), "{text}");
    }
    assert_eq!(read(&package), before);

    // Une plage qui admet la version, ou une dépendance déclarée autrement : rien n'est touché,
    // et doctor n'avertit pas.
    for value in [
        format!("{repository}#semver:>=0.0.1 <1000.0.0"),
        format!("{repository}#semver:^0.0.1 || ^{CLIENT_VERSION}"),
        "file:../drakbase/clients/tauri".to_owned(),
        "link:../drakbase/clients/tauri".to_owned(),
        "workspace:*".to_owned(),
        format!("{repository}#v0.0.1"),
    ] {
        declare("devDependencies", &value);
        let before = read(&package);
        let text = init();
        assert!(
            text.contains("· package.json : déjà en place"),
            "{value}\n{text}"
        );
        assert!(!text.contains("! @drakbase/tauri"), "{value}\n{text}");
        assert_eq!(read(&package), before, "{value}");
        let text = doctor();
        assert!(text.contains(untouched), "{value}\n{text}");
    }

    // Une plage qui demande une aide plus récente que l'outil : c'est l'outil qui est en retard,
    // et init ne ramène pas la dépendance en arrière.
    // De même pour une borne basse stricte, un blanc après l'opérateur, un intervalle ou un `v`.
    for range in [
        "^1000.0.0",
        ">1000.0.0",
        ">= 1000.0.0",
        "1000.0.0 - 1001.0.0",
        "v1000.0.0",
    ] {
        declare("devDependencies", &format!("{repository}#semver:{range}"));
        let before = read(&package);
        let text = init();
        let expected = format!(
            "! @drakbase/tauri plus récent que l'outil : la plage {range} de package.json commence après la version {CLIENT_VERSION}"
        );
        assert!(text.contains(&expected), "{expected}\n{text}");
        assert!(!text.contains("monté de"), "{range}\n{text}");
        assert!(!text.contains("remis à"), "{range}\n{text}");
        assert_eq!(read(&package), before, "{range}");
        let text = doctor();
        for expected in [
            expected.as_str(),
            "  → mets l'outil drakbase à jour, puis relance drakbase doctor",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
    }
}
