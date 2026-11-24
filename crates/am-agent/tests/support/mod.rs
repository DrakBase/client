//! Aides des essais du binaire `drakbase-stack` : une machine simulée dans un dossier temporaire
//! (dossiers de l'agent, faux `docker` et faux `systemctl` qui journalisent leurs appels), des
//! versions de la stack « demo » construites et signées à la volée avec les clés d'essai, et le
//! faux serveur. Aucun essai n'écrit hors de son dossier temporaire ni ne lance le vrai Docker.
#![allow(dead_code)]

pub mod fake_server;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use am_protocol::stack::{Digest, archive};
use am_protocol::{Arch, Version, signing};
use serde_json::{Value, json};
use tempfile::TempDir;

use fake_server::{FakeRelease, FakeServer};

/// Mot de passe public des clés de `fixtures/keys`.
pub const FIXTURE_PASSWORD: &str = "appsmarket-fixtures";
pub const APP: &str = "demo";

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Clé d'essai au format Tauri (`test-app.pub`, `other-app.secret`…).
pub fn key(name: &str) -> String {
    fs::read_to_string(fixtures().join("keys").join(name))
        .unwrap()
        .trim()
        .to_owned()
}

/// Arch de cette machine, celle que l'agent demande.
pub fn arch() -> Arch {
    match std::env::consts::ARCH {
        "aarch64" => Arch::Aarch64,
        "x86_64" => Arch::X86_64,
        other => panic!("arch d'essai non prise en charge : {other}"),
    }
}

pub fn sign(bytes: &[u8], version: &str, secret: &str) -> String {
    signing::sign(
        &key(secret),
        FIXTURE_PASSWORD,
        bytes,
        "demo.stack.tar.gz",
        &Version::parse_release(version).unwrap(),
        1_767_225_600,
    )
    .unwrap()
}

const FAKE_DOCKER: &str = r#"#!/bin/sh
# Faux docker des essais de drakbase-stack : journalise ses appels et rend des sorties préparées.
D='@DIR@'
printf '%s\n' "$*" >> "$D/calls.log"
case "$1" in
  version)
    if [ -f "$D/daemon-down" ]; then
      echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock" >&2
      exit 1
    fi
    cat "$D/engine"
    exit 0 ;;
  image) exit 0 ;;
  compose) shift ;;
  *) echo "faux docker : commande $1 inconnue" >&2; exit 1 ;;
esac
dir=''
env=''
while [ $# -gt 0 ]; do
  case "$1" in
    --project-name|-f|--profile) shift 2 ;;
    --env-file) env=$2; shift 2 ;;
    --project-directory) dir=$2; shift 2 ;;
    *) break ;;
  esac
done
action=$1
v=$(basename "$dir")
# Dossier où une version est examinée avant d'être mise en place : `.<version>.next`.
case "$v" in
  .*.next) v=${v#.}; v=${v%.next} ;;
esac
case "$action" in
  version) cat "$D/compose" ;;
  ls) cat "$D/projects.json" ;;
  config)
    if [ -f "$D/fail/config-$v" ]; then echo "config : échec simulé pour $v" >&2; exit 1; fi
    resolved="$D/config/$v.json"
    # Une forme résolue qui dépend d'un réglage : celle de `<version>.then.json` quand le .env
    # contient la ligne de `<version>.if`.
    if [ -f "$D/config/$v.if" ] && grep -qxF "$(cat "$D/config/$v.if")" "$env"; then
      resolved="$D/config/$v.then.json"
    fi
    sed "s|@PROJECT_DIR@|$dir|g" "$resolved" ;;
  pull|run|up)
    # Un .env changé à la main pendant l'opération : la ligne préparée y est ajoutée.
    if [ -f "$D/edit-env/$action-$v" ]; then cat "$D/edit-env/$action-$v" >> "$env"; fi
    if [ -f "$D/fail/$action-$v" ]; then echo "$action : échec simulé pour $v" >&2; exit 1; fi
    echo "$action $v" >> "$D/applied.log" ;;
  logs) echo "journal simulé de $v" ;;
  ps) cat "$D/ps.json" ;;
  down) echo "down $v $*" >> "$D/applied.log" ;;
  *) echo "faux docker : compose $action inconnu" >&2; exit 1 ;;
esac
"#;

const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> '@DIR@/systemctl.log'
"#;

/// Une machine Linux simulée : les dossiers de l'agent et un faux Docker.
pub struct Machine {
    pub root: TempDir,
}

impl Machine {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let machine = Self { root };
        let docker = machine.docker_dir();
        fs::create_dir_all(docker.join("config")).unwrap();
        fs::create_dir_all(docker.join("fail")).unwrap();
        write_script(
            &machine.root.path().join("docker"),
            &FAKE_DOCKER.replace("@DIR@", docker.to_str().unwrap()),
        );
        write_script(
            &machine.root.path().join("systemctl"),
            &FAKE_SYSTEMCTL.replace("@DIR@", docker.to_str().unwrap()),
        );
        machine.set_engine("29.1.3");
        machine.set_compose("2.29.1");
        fs::write(docker.join("projects.json"), "[]\n").unwrap();
        fs::write(
            docker.join("ps.json"),
            "{\"Service\":\"app\",\"State\":\"running\",\"Health\":\"healthy\"}\n",
        )
        .unwrap();
        machine
    }

    pub fn docker_dir(&self) -> PathBuf {
        self.root.path().join("faux-docker")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("etc")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("lib")
    }

    pub fn systemd_dir(&self) -> PathBuf {
        self.root.path().join("systemd")
    }

    pub fn stack_dir(&self) -> PathBuf {
        self.state_dir().join(APP)
    }

    pub fn release_dir(&self, version: &str) -> PathBuf {
        self.stack_dir().join("releases").join(version)
    }

    /// Le binaire, avec les dossiers et les commandes de cette machine, sans terminal ni proxy.
    pub fn agent(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_drakbase-stack"));
        command
            .env("DRAKBASE_STACK_CONFIG_DIR", self.config_dir())
            .env("DRAKBASE_STACK_STATE_DIR", self.state_dir())
            .env("DRAKBASE_STACK_SYSTEMD_DIR", self.systemd_dir())
            .env("DRAKBASE_STACK_DOCKER", self.root.path().join("docker"))
            .env(
                "DRAKBASE_STACK_SYSTEMCTL",
                self.root.path().join("systemctl"),
            )
            .stdin(Stdio::null());
        for proxy in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(proxy);
        }
        command
    }

    pub fn run(&self, args: &[&str]) -> Run {
        let output = self.agent().args(args).output().unwrap();
        Run { output }
    }

    /// `install demo --key <test-app.pub> --server <faux serveur> --accept`, qui doit réussir.
    pub fn install(&self, server: &FakeServer) -> Run {
        let run = self.run(&[
            "install",
            APP,
            "--key",
            &key("test-app.pub"),
            "--server",
            &server.url,
            "--accept",
        ]);
        run.assert_code(0);
        run
    }

    pub fn set_engine(&self, version: &str) {
        fs::write(self.docker_dir().join("engine"), format!("{version}\n")).unwrap();
    }

    pub fn set_compose(&self, version: &str) {
        fs::write(self.docker_dir().join("compose"), format!("{version}\n")).unwrap();
    }

    pub fn set_projects(&self, json: &str) {
        fs::write(self.docker_dir().join("projects.json"), json).unwrap();
    }

    pub fn daemon_down(&self, down: bool) {
        let path = self.docker_dir().join("daemon-down");
        if down {
            fs::write(path, "").unwrap();
        } else {
            let _ = fs::remove_file(path);
        }
    }

    /// Fait échouer `pull`, `run`, `up` ou `config` pour une version.
    pub fn fail(&self, action: &str, version: &str) {
        fs::write(
            self.docker_dir()
                .join("fail")
                .join(format!("{action}-{version}")),
            "",
        )
        .unwrap();
    }

    /// Ajoute `line` au `.env` pendant `pull`, `run` ou `up` d'une version, comme une personne qui
    /// le changerait à la main pendant la mise à jour.
    pub fn edit_env_during(&self, action: &str, version: &str, line: &str) {
        let dir = self.docker_dir().join("edit-env");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{action}-{version}")), line).unwrap();
    }

    pub fn heal(&self, action: &str, version: &str) {
        let _ = fs::remove_file(
            self.docker_dir()
                .join("fail")
                .join(format!("{action}-{version}")),
        );
    }

    /// Appels faits au faux docker, un par ligne (arguments séparés par des espaces).
    pub fn calls(&self) -> Vec<String> {
        read_lines(&self.docker_dir().join("calls.log"))
    }

    /// Ce que le faux docker a appliqué : `pull 1.1.0`, `run 1.1.0`, `up 1.0.0`, `down …`.
    pub fn applied(&self) -> Vec<String> {
        read_lines(&self.docker_dir().join("applied.log"))
    }

    pub fn systemctl_calls(&self) -> Vec<String> {
        read_lines(&self.docker_dir().join("systemctl.log"))
    }

    pub fn clear_logs(&self) {
        for name in ["calls.log", "applied.log", "systemctl.log"] {
            let _ = fs::remove_file(self.docker_dir().join(name));
        }
    }

    /// `status.json`, ce que lit l'app.
    pub fn status(&self) -> Value {
        serde_json::from_slice(&fs::read(self.stack_dir().join("control/status.json")).unwrap())
            .unwrap()
    }

    /// Dépose une demande, comme le fait l'app : sous un nom en `.…`, puis renommée.
    pub fn request(&self, name: &str, body: &str) {
        let dir = self.stack_dir().join("control/request");
        let temp = dir.join(format!(".{name}"));
        fs::write(&temp, body).unwrap();
        fs::rename(&temp, dir.join(name)).unwrap();
    }

    pub fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.stack_dir().join("state.json")).unwrap()).unwrap()
    }

    pub fn config(&self) -> Value {
        serde_json::from_slice(&fs::read(self.config_dir().join("config.json")).unwrap()).unwrap()
    }

    pub fn env_file(&self) -> String {
        fs::read_to_string(self.stack_dir().join(".env")).unwrap()
    }

    /// Versions présentes dans `releases/`.
    pub fn releases(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.stack_dir().join("releases"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Prépare la sortie de `docker compose config` de cette version, puis la publie.
    pub fn publish(&self, server: &FakeServer, release: &Release) {
        self.prepare(server, release);
        server.publish(release.fake_release());
    }

    /// Prépare la sortie de `docker compose config` de cette version quand le `.env` contient la
    /// ligne `setting` (`NOM=valeur`) : un réglage qui change la forme résolue du compose.
    pub fn prepare_when(&self, server: &FakeServer, setting: &str, release: &Release) {
        let host = server.url.trim_start_matches("http://");
        let config = self.docker_dir().join("config");
        fs::write(config.join(format!("{}.if", release.version)), setting).unwrap();
        fs::write(
            config.join(format!("{}.then.json", release.version)),
            release.compose_json(host),
        )
        .unwrap();
    }

    /// Prépare seulement la sortie de `docker compose config` de cette version.
    pub fn prepare(&self, server: &FakeServer, release: &Release) {
        let host = server.url.trim_start_matches("http://");
        fs::write(
            self.docker_dir()
                .join("config")
                .join(format!("{}.json", release.version)),
            release.compose_json(host),
        )
        .unwrap();
    }
}

fn write_script(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Résultat d'une commande de l'agent.
pub struct Run {
    pub output: Output,
}

impl Run {
    pub fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// Les deux sorties, pour les messages d'échec et les recherches.
    pub fn text(&self) -> String {
        format!("{}{}", self.stdout(), self.stderr())
    }

    pub fn assert_code(&self, code: i32) {
        assert_eq!(
            self.code(),
            Some(code),
            "code de sortie inattendu\n--- sortie ---\n{}\n--- erreurs ---\n{}",
            self.stdout(),
            self.stderr()
        );
    }

    pub fn assert_contains(&self, needle: &str) {
        let text = self.text();
        assert!(text.contains(needle), "« {needle} » absent de :\n{text}");
    }
}

/// Une version de la stack « demo » : `stack.json`, compose, `.env.example` et `files/`, avec
/// la sortie de `docker compose config` qu'en donnerait Docker Compose.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub rollback: &'static str,
    pub pre_update: bool,
    pub ports: Vec<u16>,
    pub cap_add: Vec<String>,
    pub env_example: String,
    pub public_keys: Vec<String>,
    /// Clé secrète qui signe l'archive, et version écrite dans la signature.
    pub signed_by: &'static str,
    pub signed_version: String,
    /// Archive servie à la place de celle de cette version.
    pub bytes: Option<Vec<u8>>,
    pub beta: bool,
    pub always: bool,
    pub notes: Option<String>,
    /// `app_control` de `stack.json` : l'app propose ses mises à jour dans son interface.
    pub app_control: Option<&'static str>,
    /// `compose.yaml` de l'archive, à la place du compose publié par l'outil.
    pub compose: Option<String>,
}

/// Le `compose.yaml` d'une archive, comme l'outil l'écrit : un document JSON, rien d'autre. Le
/// faux docker ne le lit pas : il rend la forme résolue préparée par le test.
pub const PUBLISHED_COMPOSE: &str =
    "{\n  \"services\": {\n    \"app\": { \"image\": \"(forme publiée)\" }\n  }\n}\n";

impl Release {
    /// Une version dont l'app propose ses mises à jour dans son interface : `app_control`.
    pub fn with_control(version: &str) -> Self {
        Self {
            app_control: Some("/run/drakbase"),
            ..Self::new(version)
        }
    }

    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            rollback: "manual",
            pre_update: false,
            ports: vec![8080],
            cap_add: Vec::new(),
            env_example: "# Port de l'interface\nPORT=8080\n".to_owned(),
            public_keys: vec![key("test-app.pub")],
            signed_by: "test-app.secret",
            signed_version: version.to_owned(),
            bytes: None,
            beta: false,
            always: false,
            notes: None,
            app_control: None,
            compose: None,
        }
    }

    /// Empreinte de l'image de cette version.
    pub fn digest(&self) -> Digest {
        Digest::of(self.version.as_bytes())
    }

    pub fn image(&self, host: &str) -> String {
        format!("{host}/{APP}/app@{}", self.digest())
    }

    pub fn stack_json(&self) -> String {
        let mut value = json!({
            "format": 1,
            "app": APP,
            "version": self.version,
            "service": "app",
            "rollback": self.rollback,
            "health_timeout": 120,
            "public_keys": self.public_keys,
            "images": { "app": self.digest().as_str() },
        });
        if self.pre_update {
            value["pre_update"] = json!("backup");
        }
        if let Some(path) = self.app_control {
            value["app_control"] = json!(path);
        }
        serde_json::to_string_pretty(&value).unwrap()
    }

    pub fn archive(&self) -> Vec<u8> {
        if let Some(bytes) = &self.bytes {
            return bytes.clone();
        }
        let mut files = BTreeMap::new();
        files.insert("stack.json".to_owned(), self.stack_json().into_bytes());
        files.insert(
            "compose.yaml".to_owned(),
            self.compose
                .as_deref()
                .unwrap_or(PUBLISHED_COMPOSE)
                .as_bytes()
                .to_vec(),
        );
        files.insert(
            ".env.example".to_owned(),
            self.env_example.clone().into_bytes(),
        );
        files.insert("files/app.conf".to_owned(), b"port = 8080\n".to_vec());
        archive::write(&files).unwrap()
    }

    /// Forme résolue du compose, `@PROJECT_DIR@` valant le dossier du projet.
    pub fn compose_json(&self, host: &str) -> String {
        let ports: Vec<Value> = self
            .ports
            .iter()
            .map(|port| {
                json!({"mode": "ingress", "target": port, "published": port.to_string(), "protocol": "tcp"})
            })
            .collect();
        let mut app = json!({
            "image": self.image(host),
            "ports": ports,
            "volumes": [
                {"type": "bind", "source": "@PROJECT_DIR@/files/app.conf", "target": "/etc/app.conf", "read_only": true},
                {"type": "volume", "source": "data", "target": "/data", "volume": {}}
            ]
        });
        if !self.cap_add.is_empty() {
            app["cap_add"] = json!(self.cap_add);
        }
        let mut services = json!({ "app": app });
        if self.pre_update {
            services["backup"] = json!({
                "image": self.image(host),
                "profiles": ["drakbase-hooks"],
                "command": ["backup"],
                "volumes": [{"type": "volume", "source": "data", "target": "/data", "volume": {}}]
            });
        }
        serde_json::to_string_pretty(&json!({
            "name": APP,
            "services": services,
            "volumes": {"data": {"name": "demo_data"}}
        }))
        .unwrap()
    }

    pub fn fake_release(&self) -> FakeRelease {
        let bytes = self.archive();
        let signature = sign(&bytes, &self.signed_version, self.signed_by);
        let mut release = FakeRelease::stack(APP, &self.version, arch(), bytes, signature);
        if self.beta {
            release = release.beta();
        }
        if self.always {
            release = release.always();
        }
        if let Some(notes) = &self.notes {
            release = release.notes(notes);
        }
        release
    }
}
