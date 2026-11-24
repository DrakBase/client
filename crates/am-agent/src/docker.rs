//! Docker, appelé par ses commandes (`docker`, `docker compose`). La commande est réglable
//! (`DRAKBASE_STACK_DOCKER`) : les essais la remplacent par un faux docker qui journalise ses
//! appels et rend des sorties préparées.
//!
//! Docker Compose ne reçoit que des variables d'environnement choisies : une variable du shell
//! l'emporte sur le `.env` dans l'interpolation, et `COMPOSE_FILE`, `COMPOSE_PROFILES` ou
//! `COMPOSE_PROJECT_NAME` changeraient le projet. Le compose contrôlé est ainsi celui qui démarre,
//! que l'agent soit lancé à la main ou par le minuteur.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

use am_protocol::OsVersion;
use am_protocol::stack::HOOKS_PROFILE;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::paths::{ENV_DOCKER, StackPaths, program_from_env};

/// Docker Engine 24 et Docker Compose 2.29 au moins, les plus anciens essayés en S0
/// (docs/PROTOCOLE.md, « Prérequis »).
pub const MIN_ENGINE: &str = "24.0";
pub const MIN_COMPOSE: &str = "2.29";

/// Variables transmises à Docker : de quoi trouver le démon et ses réglages, sans rien qui
/// change l'interpolation du compose au-delà des variables usuelles du système. Un compose peut
/// lire chacune d'elles et la donner à son conteneur (`${HOME}`, `environment: ["HOME"]`) :
/// aucune ne porte de secret. Les proxys et `DOCKER_AUTH_CONFIG`, qui portent des mots de passe,
/// n'en sont donc pas.
fn passes_through(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "LANG"
            | "TERM"
            | "NO_COLOR"
            | "TMPDIR"
            | "XDG_RUNTIME_DIR"
            | "DOCKER_HOST"
            | "DOCKER_CONTEXT"
            | "DOCKER_CONFIG"
            | "DOCKER_TLS_VERIFY"
            | "DOCKER_CERT_PATH"
            | "DOCKER_API_VERSION"
            | "DOCKER_DEFAULT_PLATFORM"
    ) || name.starts_with("LC_")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerVersions {
    pub engine: String,
    /// Sans `v` initial : c'est l'`os_version` envoyée au serveur.
    pub compose: String,
}

/// Un projet Compose de l'agent : `--project-name <app> --project-directory <dossier de la
/// version> --env-file <.env> -f <dossier>/compose.yaml`.
#[derive(Clone, Copy, Debug)]
pub struct Project<'a> {
    pub name: &'a str,
    pub dir: &'a Path,
    pub env_file: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    pub service: String,
    pub state: String,
    pub health: String,
}

pub struct Docker {
    program: OsString,
}

impl Docker {
    pub fn from_env() -> Self {
        Self {
            program: program_from_env(ENV_DOCKER, "docker"),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.env_clear();
        for (name, value) in env::vars_os() {
            if passes_through(&name) {
                command.env(name, value);
            }
        }
        command.stdin(Stdio::null());
        command
    }

    fn spawn_error(&self, error: io::Error) -> anyhow::Error {
        if error.kind() == io::ErrorKind::NotFound {
            anyhow!(
                "commande {} introuvable : installez Docker Engine {MIN_ENGINE} ou plus (https://docs.docker.com/engine/install/)",
                self.program.to_string_lossy()
            )
        } else {
            anyhow::Error::new(error)
                .context(format!("lancement de {}", self.program.to_string_lossy()))
        }
    }

    /// Lance une commande et rend sa sortie standard ; en cas d'échec, l'erreur reprend la fin
    /// de sa sortie d'erreur.
    fn capture(&self, mut command: Command, what: &str) -> Result<Vec<u8>> {
        let output = command.output().map_err(|error| self.spawn_error(error))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(anyhow!("{what} a échoué{}", error_tail(&output)))
        }
    }

    /// Lance une commande dont la sortie va directement au terminal (ou au journal).
    fn inherit(&self, mut command: Command, what: &str) -> Result<()> {
        let status = command.status().map_err(|error| self.spawn_error(error))?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("{what} a échoué ({})", describe_status(status)))
        }
    }

    pub fn engine_version(&self) -> Result<String> {
        let mut command = self.command();
        command.args(["version", "--format", "{{.Server.Version}}"]);
        let output = command.output().map_err(|error| self.spawn_error(error))?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || version.is_empty() {
            bail!(
                "le service Docker ne répond pas (docker version){} ; est-il démarré ?",
                error_tail(&output)
            );
        }
        Ok(version)
    }

    pub fn compose_version(&self) -> Result<String> {
        let mut command = self.command();
        command.args(["compose", "version", "--short"]);
        let output = command.output().map_err(|error| self.spawn_error(error))?;
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || version.is_empty() {
            bail!(
                "Docker Compose est introuvable (docker compose version){} : installez le greffon docker-compose-plugin, {MIN_COMPOSE} ou plus",
                error_tail(&output)
            );
        }
        Ok(version.trim_start_matches(['v', 'V']).to_owned())
    }

    /// Contrôle des prérequis, avant toute opération sur une stack.
    pub fn prerequisites(&self) -> Result<DockerVersions> {
        let engine = self.engine_version()?;
        if !at_least(&engine, MIN_ENGINE) {
            bail!(
                "Docker Engine {engine} est trop ancien : drakbase-stack demande Docker Engine {MIN_ENGINE} ou plus"
            );
        }
        let compose = self.compose_version()?;
        if !at_least(&compose, MIN_COMPOSE) {
            bail!(
                "Docker Compose {compose} est trop ancien : drakbase-stack demande Docker Compose {MIN_COMPOSE} ou plus"
            );
        }
        Ok(DockerVersions { engine, compose })
    }

    /// `docker compose` sur le compose de la stack, et le fichier de l'agent qui monte le
    /// dossier de contrôle quand la version en a un.
    fn compose(&self, project: &Project<'_>) -> Command {
        let mut command = self.compose_stack_only(project);
        let control = StackPaths::control_compose_file(project.dir);
        if control.is_file() {
            command.arg("-f").arg(control);
        }
        command
    }

    /// `docker compose` sur le seul compose de la stack : le contrôle des permissions ne voit pas
    /// le montage qu'ajoute l'agent.
    fn compose_stack_only(&self, project: &Project<'_>) -> Command {
        let mut command = self.command();
        command
            .arg("compose")
            .arg("--project-name")
            .arg(project.name)
            .arg("--project-directory")
            .arg(project.dir)
            .arg("--env-file")
            .arg(project.env_file)
            .arg("-f")
            .arg(StackPaths::compose_file(project.dir));
        command
    }

    /// Forme résolue du compose, tous profils compris (docs/PROTOCOLE.md, « Compose »).
    pub fn config(&self, project: &Project<'_>) -> Result<Vec<u8>> {
        let mut command = self.compose_stack_only(project);
        command.args(["--profile", "*", "config", "--format", "json"]);
        self.capture(command, "docker compose config")
    }

    /// Tire les images de tous les services, `pre_update` compris.
    pub fn pull(&self, project: &Project<'_>) -> Result<()> {
        let mut command = self.compose(project);
        command.args(["--profile", "*", "pull"]);
        self.inherit(command, "docker compose pull")
    }

    /// Lance le service de préparation, puis le retire.
    pub fn run_hook(&self, project: &Project<'_>, service: &str) -> Result<()> {
        let mut command = self.compose(project);
        command
            .args(["--profile", HOOKS_PROFILE, "run", "--rm", "-T"])
            .arg(service);
        self.inherit(command, &format!("docker compose run {service}"))
    }

    /// Démarre la version et attend l'état sain de ses services.
    pub fn up(&self, project: &Project<'_>, health_timeout: u32) -> Result<()> {
        let mut command = self.compose(project);
        command
            .args(["up", "-d", "--remove-orphans", "--wait", "--wait-timeout"])
            .arg(health_timeout.to_string());
        self.inherit(command, "docker compose up")
    }

    pub fn down(&self, project: &Project<'_>, volumes: bool) -> Result<()> {
        let mut command = self.compose(project);
        command.args(["down", "--remove-orphans"]);
        if volumes {
            command.arg("--volumes");
        }
        self.inherit(command, "docker compose down")
    }

    /// Fin du journal de tous les services, pour l'expliquer après un échec.
    pub fn logs_tail(&self, project: &Project<'_>, lines: u32) -> String {
        let mut command = self.compose(project);
        command
            .args(["logs", "--no-color", "--tail"])
            .arg(lines.to_string());
        match command.output() {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                text
            }
            Err(error) => format!("(journal illisible : {error})\n"),
        }
    }

    pub fn logs(&self, project: &Project<'_>, follow: bool, tail: u32) -> Result<()> {
        let mut command = self.compose(project);
        command.args(["logs", "--tail"]).arg(tail.to_string());
        if follow {
            command.arg("--follow");
        }
        self.inherit(command, "docker compose logs")
    }

    pub fn ps(&self, project: &Project<'_>) -> Result<Vec<ServiceStatus>> {
        let mut command = self.compose(project);
        command.args(["ps", "--all", "--format", "json"]);
        let output = self.capture(command, "docker compose ps")?;
        parse_ps(&String::from_utf8_lossy(&output))
    }

    /// Noms des projets Compose de la machine, arrêtés compris.
    pub fn projects(&self) -> Result<Vec<String>> {
        let mut command = self.command();
        command.args(["compose", "ls", "--all", "--format", "json"]);
        let output = self.capture(command, "docker compose ls")?;
        let value: Value =
            serde_json::from_slice(&output).context("réponse de docker compose ls illisible")?;
        Ok(value
            .as_array()
            .map(|projects| {
                projects
                    .iter()
                    .filter_map(|project| project.get("Name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Supprime des images, sans s'arrêter sur celles qui sont encore utilisées ou déjà parties.
    pub fn remove_images(&self, images: &[String]) {
        if images.is_empty() {
            return;
        }
        let mut command = self.command();
        command.args(["image", "rm"]).args(images);
        let _ = command.output();
    }
}

/// Fin de la sortie d'erreur d'une commande, précédée de « : », ou rien.
fn error_tail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(5)..].join(" / ");
    if tail.is_empty() {
        format!(" ({})", describe_status(output.status))
    } else {
        format!(" : {tail}")
    }
}

fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("code {code}"),
        None => "interrompu par un signal".to_owned(),
    }
}

/// `version` vaut-elle au moins `minimum` ? Une version illisible ne le vaut pas. Les deux se
/// lisent comme des versions d'OS (docs/PROTOCOLE.md, « Versions d'OS ») : `24.0.9-ce` vaut
/// 24.0.9.
pub fn at_least(version: &str, minimum: &str) -> bool {
    match (OsVersion::parse(version), OsVersion::parse(minimum)) {
        (Some(version), Some(minimum)) => version.at_least(&minimum),
        _ => false,
    }
}

/// Sortie de `docker compose ps --format json` : un objet JSON par ligne, comme l'écrit Docker
/// Compose depuis la 2.21 (`MIN_COMPOSE` est au-delà).
pub fn parse_ps(text: &str) -> Result<Vec<ServiceStatus>> {
    let values: Vec<Map<String, Value>> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .context("réponse de docker compose ps illisible")?;
    let field = |value: &Map<String, Value>, name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let mut statuses: Vec<ServiceStatus> = values
        .iter()
        .map(|value| ServiceStatus {
            service: field(value, "Service"),
            state: field(value, "State"),
            health: field(value, "Health"),
        })
        .collect();
    statuses.sort_by(|a, b| a.service.cmp(&b.service));
    Ok(statuses)
}

/// Images des services d'un compose résolu, triées et sans doublon.
pub fn images_of(config_json: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(config_json) else {
        return Vec::new();
    };
    let mut images: Vec<String> = value
        .get("services")
        .and_then(Value::as_object)
        .map(|services| {
            services
                .values()
                .filter_map(|service| service.get("image").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    images.sort();
    images.dedup();
    images
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_to_the_minimums() {
        assert!(at_least("29.1.3", MIN_ENGINE));
        assert!(at_least("24.0.0", MIN_ENGINE));
        assert!(at_least("24.0.9-ce", MIN_ENGINE));
        assert!(!at_least("23.0.17", MIN_ENGINE));
        assert!(!at_least("20.10.24+dfsg1", MIN_ENGINE));
        assert!(at_least("2.29.1", MIN_COMPOSE));
        assert!(at_least("2.29.1-desktop.1", MIN_COMPOSE));
        assert!(at_least("5.7.0", MIN_COMPOSE));
        assert!(!at_least("2.28.9", MIN_COMPOSE));
        assert!(!at_least("illisible", MIN_COMPOSE));
        // Le « v » de Docker Compose est retiré par `compose_version`, pas ici.
        assert!(!at_least("v2.29.1", MIN_COMPOSE));
    }

    #[test]
    fn only_chosen_variables_reach_docker() {
        for allowed in ["PATH", "HOME", "DOCKER_HOST", "DOCKER_CONFIG", "LC_ALL"] {
            assert!(passes_through(OsStr::new(allowed)), "{allowed}");
        }
        for refused in [
            // Des mots de passe, qu'un compose ferait entrer dans son conteneur.
            "DOCKER_AUTH_CONFIG",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "COMPOSE_FILE",
            "COMPOSE_PROFILES",
            "COMPOSE_PROJECT_NAME",
            "TZ",
            "PORT",
        ] {
            assert!(!passes_through(OsStr::new(refused)), "{refused}");
        }
    }

    #[test]
    fn ps_output_is_one_object_per_line() {
        let lines = "{\"Service\":\"web\",\"State\":\"running\",\"Health\":\"healthy\"}\n\n{\"Service\":\"db\",\"State\":\"exited\"}\n";
        let statuses = parse_ps(lines).unwrap();
        assert_eq!(statuses[0].service, "db");
        assert_eq!(statuses[0].health, "");
        assert_eq!(statuses[1].state, "running");
        assert_eq!(statuses[1].health, "healthy");
        assert!(parse_ps("").unwrap().is_empty());
        assert!(parse_ps("pas du JSON").is_err());
        // Le tableau qu'écrivait Docker Compose avant la 2.21 n'est plus lu.
        assert!(parse_ps("[{\"Service\":\"web\",\"State\":\"running\"}]").is_err());
    }

    #[test]
    fn images_of_the_resolved_compose() {
        let config = br#"{"services":{"b":{"image":"x@sha256:1"},"a":{"image":"x@sha256:1"},"c":{"image":"y@sha256:2"}}}"#;
        assert_eq!(images_of(config), ["x@sha256:1", "y@sha256:2"]);
    }
}
