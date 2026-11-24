//! Faux serveur de l'essai réel sous Linux (`crates/am-agent/trial/linux.sh`) : il sert la stack
//! « demo », signée par la clé d'essai de `fixtures/keys`, et son image en registre, depuis un
//! dossier OCI. Rien n'est gardé sur le disque.
//!
//! ```text
//! cargo run -p am-agent --example trial_server -- <écoute> <adresse publique> <dossier OCI> <version>…
//! ```
//!
//! Versions publiées à la demande, pour aarch64 et x86_64. Toutes ont `app_control`
//! (`/run/drakbase`) : l'app reçoit le dossier de contrôle de ses mises à jour.
//! - `1.0.0` : saine ;
//! - `1.1.0` : une variable de plus dans `.env.example` (`TZ`), et une sauvegarde avant mise à
//!   jour (`pre_update`) ;
//! - `1.2.0` : ne devient jamais saine, avec `rollback: safe` ; elle ajoute un réglage (`EXTRA`),
//!   qui ne doit pas rester dans `.env` après le retour à la version précédente ;
//! - `1.3.0` : demande une capacité de plus, donc une permission à accepter ;
//! - `1.4.0` : avec `rollback: manual`, ne devient saine qu'avec le réglage `HEALTH=true` dans
//!   `.env`, alors que son `.env.example` donne `false`.

#[path = "../tests/support/fake_server.rs"]
mod fake_server;

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::stack::archive;
use am_protocol::{Arch, Version, signing};
use fake_server::{FakeRelease, FakeServer};
use serde_json::{Value, json};

const APP: &str = "demo";
/// Chemin du dossier de contrôle dans le conteneur de l'app.
const APP_CONTROL: &str = "/run/drakbase";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [listen, public, oci, versions @ ..] = args.as_slice() else {
        eprintln!(
            "utilisation : trial_server <écoute> <adresse publique> <dossier OCI> <version>…\n  par exemple : trial_server 127.0.0.1:8790 http://host.docker.internal:8790 /tmp/oci 1.0.0 1.1.0"
        );
        return ExitCode::from(2);
    };
    let address: SocketAddr = match listen.parse() {
        Ok(address) => address,
        Err(error) => {
            eprintln!("adresse d'écoute « {listen} » : {error}");
            return ExitCode::from(2);
        }
    };
    let digest = match image_digest(Path::new(oci)) {
        Ok(digest) => digest,
        Err(error) => {
            eprintln!("{oci}/index.json : {error}");
            return ExitCode::FAILURE;
        }
    };
    let host = public
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');

    // Les signatures d'abord (scrypt prend un moment) : une fois à l'écoute, le serveur sert
    // toutes ses versions.
    let releases: Vec<FakeRelease> = versions
        .iter()
        .flat_map(|version| {
            let archive = stack_archive(version, host, &digest);
            let signature = sign(&archive, version);
            [Arch::Aarch64, Arch::X86_64].map(|arch| {
                FakeRelease::stack(APP, version, arch, archive.clone(), signature.clone())
            })
        })
        .collect();
    let server = FakeServer::bind(address);
    server.set_public_url(public);
    server.serve_oci(APP, PathBuf::from(oci));
    server.declare(APP);
    for release in releases {
        server.publish(release);
    }
    println!(
        "faux serveur prêt sur {listen} ({public}), image {host}/{APP}/app@{digest}, versions : {}",
        versions.join(", ")
    );
    loop {
        std::thread::park();
    }
}

/// Empreinte de l'image du dossier OCI : la première entrée de `index.json`.
fn image_digest(layout: &Path) -> Result<String, String> {
    let index: Value = serde_json::from_slice(
        &fs::read(layout.join("index.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    index["manifests"][0]["digest"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "aucun manifeste".to_owned())
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn key(name: &str) -> String {
    fs::read_to_string(fixtures().join("keys").join(name))
        .unwrap_or_else(|error| panic!("clé d'essai {name} : {error}"))
        .trim()
        .to_owned()
}

fn sign(bytes: &[u8], version: &str) -> String {
    signing::sign(
        &key("test-app.secret"),
        "appsmarket-fixtures",
        bytes,
        &format!("{APP}-{version}.stack.tar.gz"),
        &Version::parse_release(version).expect("version"),
        1_767_225_600,
    )
    .expect("signature d'essai")
}

fn stack_archive(version: &str, host: &str, digest: &str) -> Vec<u8> {
    let with_backup = version != "1.0.0";
    // Deux versions qui ne deviennent pas saines : la 1.2.0 jamais, la 1.4.0 tant que le réglage
    // HEALTH de .env vaut `false`.
    let never_healthy = version == "1.2.0";
    let healthy_by_setting = version == "1.4.0";
    let mut manifest = json!({
        "format": 1,
        "app": APP,
        "version": version,
        "service": "app",
        "health_timeout": if never_healthy || healthy_by_setting { 15 } else { 60 },
        "rollback": if never_healthy { "safe" } else { "manual" },
        "public_keys": [key("test-app.pub")],
        "images": { "app": digest },
        "app_control": APP_CONTROL,
    });
    if with_backup {
        manifest["pre_update"] = json!("backup");
    }
    // Le contrôle de santé est une commande : `true`, sauf réglage contraire dans .env.
    let health = if never_healthy {
        "false"
    } else {
        "${HEALTH:-true}"
    };
    let image = format!("{host}/{APP}/app@{digest}");
    // `$$` : Compose le rend en `$`, que lit le shell du conteneur.
    let start =
        format!("cat /etc/demo/app.conf; echo \"demo {version}, TZ=$${{TZ}}\"; exec sleep 86400");
    let mut app = json!({
        "image": image,
        "command": ["sh", "-c", start],
        "environment": { "TZ": "${TZ:-UTC}" },
        "healthcheck": {
            "test": ["CMD", health],
            "interval": "2s",
            "timeout": "2s",
            "retries": 2,
            "start_period": "1s",
        },
        "restart": "unless-stopped",
        "ports": ["${PORT:-8080}:8080"],
        "volumes": ["./files/app.conf:/etc/demo/app.conf:ro", "data:/data"],
    });
    if version == "1.3.0" {
        app["cap_add"] = json!(["NET_ADMIN"]);
    }
    let mut services = json!({ "app": app });
    if with_backup {
        services["backup"] = json!({
            "image": image,
            "profiles": ["drakbase-hooks"],
            "command": ["sh", "-c", format!("date > /data/sauvegarde-avant-{version}.txt")],
            "volumes": ["data:/data"],
        });
    }
    // Comme le compose que publie l'outil : un document JSON, rien d'autre.
    let compose = format!(
        "{}\n",
        serde_json::to_string_pretty(&json!({ "services": services, "volumes": { "data": {} } }))
            .expect("compose de la stack d'essai")
    );
    let mut env_example = "# Port publié sur la machine\nPORT=8080\n".to_owned();
    if version != "1.0.0" {
        env_example.push_str("# Fuseau horaire\nTZ=Europe/Paris\n");
    }
    if never_healthy {
        env_example.push_str("# Réglage propre à cette version\nEXTRA=1\n");
    }
    if healthy_by_setting {
        env_example.push_str(
            "# Commande du contrôle de santé : la version ne devient saine qu'avec true\nHEALTH=false\n",
        );
    }
    let mut files = BTreeMap::new();
    files.insert(
        archive::STACK_JSON.to_owned(),
        serde_json::to_string_pretty(&manifest)
            .expect("stack.json")
            .into_bytes(),
    );
    files.insert(archive::COMPOSE_YAML.to_owned(), compose.into_bytes());
    files.insert(archive::ENV_EXAMPLE.to_owned(), env_example.into_bytes());
    files.insert(
        "files/app.conf".to_owned(),
        format!("configuration de demo {version}\n").into_bytes(),
    );
    archive::write(&files).expect("archive d'essai")
}
