//! Docker, pour les stacks : versions des outils, lecture du compose par Docker Compose lui-même
//! (docs/PROTOCOLE.md, « Compose »), et construction des images au format OCI par
//! `docker buildx build`.

use std::env;
use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::compose::BuildSpec;

/// Pour les essais : le programme `docker` à employer, à la place de celui du PATH.
pub const DOCKER_VAR: &str = "APPSMARKET_DOCKER";

fn program() -> OsString {
    env::var_os(DOCKER_VAR)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "docker".into())
}

/// `docker`, sans les variables qui changeraient ce que lit Docker Compose (projet, fichiers,
/// profils) ni la plateforme par défaut.
fn docker() -> Command {
    let mut command = Command::new(program());
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("COMPOSE_") {
            command.env_remove(name);
        }
    }
    command.env_remove("DOCKER_DEFAULT_PLATFORM");
    command.stdin(Stdio::null());
    command
}

/// Lance `docker` et rend sa sortie ; en cas d'échec, sa sortie d'erreur.
fn output(command: &mut Command) -> Result<Vec<u8>, String> {
    match command.output() {
        Ok(output) if output.status.success() => Ok(output.stdout),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(if stderr.is_empty() {
                format!("échec ({})", output.status)
            } else {
                stderr
            })
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Err(missing()),
        Err(error) => Err(error.to_string()),
    }
}

fn missing() -> String {
    format!(
        "{} introuvable : installe Docker (Docker Desktop, OrbStack…)",
        program().to_string_lossy()
    )
}

fn text(command: &mut Command) -> Result<String, String> {
    output(command).map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// Versions de Docker (le moteur), de buildx et de Docker Compose, ou ce qui les empêche.
pub struct Tools {
    pub engine: Result<String, String>,
    pub buildx: Result<String, String>,
    pub compose: Result<String, String>,
}

impl Tools {
    pub fn detect() -> Self {
        if let Err(error) = docker().arg("--version").output()
            && error.kind() == ErrorKind::NotFound
        {
            return Self {
                engine: Err(missing()),
                buildx: Err(missing()),
                compose: Err(missing()),
            };
        }
        let engine = text(docker().args(["version", "--format", "{{.Server.Version}}"]))
            .map_err(|problem| format!("Docker ne répond pas : {}", first_line(&problem)));
        let buildx = text(docker().args(["buildx", "version"]))
            .map(|line| {
                // « github.com/docker/buildx v0.37.2 2d379c0… »
                line.split_whitespace()
                    .nth(1)
                    .unwrap_or(&line)
                    .trim_start_matches('v')
                    .to_owned()
            })
            .map_err(|problem| format!("docker buildx indisponible : {}", first_line(&problem)));
        let compose = text(docker().args(["compose", "version", "--short"]))
            .map(|version| version.trim_start_matches('v').to_owned())
            .map_err(|problem| format!("docker compose indisponible : {}", first_line(&problem)));
        Self {
            engine,
            buildx,
            compose,
        }
    }

    /// « Docker 29.8.2, buildx 0.37.2, Docker Compose 5.5.0 », ou le premier problème.
    pub fn require(&self) -> Result<String> {
        match (&self.engine, &self.buildx, &self.compose) {
            (Ok(engine), Ok(buildx), Ok(compose)) => Ok(format!(
                "Docker {engine}, buildx {buildx}, Docker Compose {compose}"
            )),
            (Err(problem), _, _) | (_, Err(problem), _) | (_, _, Err(problem)) => {
                Err(anyhow!("{problem}"))
            }
        }
    }

    /// Docker Compose seul suffit pour lire le compose.
    pub fn require_compose(&self) -> Result<()> {
        self.compose
            .as_ref()
            .map(|_| ())
            .map_err(|problem| anyhow!("{problem}"))
    }
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .trim()
}

/// Un projet Compose : son nom (le slug de l'app), son dossier et son fichier.
pub struct ComposeProject {
    pub name: String,
    pub dir: PathBuf,
    pub file: PathBuf,
    /// Fichier des variables ; sans lui, Docker Compose lit le `.env` du dossier s'il existe.
    pub env_file: Option<PathBuf>,
}

impl ComposeProject {
    fn command(&self) -> Command {
        let mut command = docker();
        command
            .arg("compose")
            .arg("-p")
            .arg(&self.name)
            .arg("--project-directory")
            .arg(&self.dir);
        if let Some(env_file) = &self.env_file {
            command.arg("--env-file").arg(env_file);
        }
        command
            .arg("-f")
            .arg(&self.file)
            .args(["--profile", "*", "config", "--format", "json"]);
        command
    }

    fn run(&self, command: &mut Command) -> Result<Vec<u8>> {
        output(command).map_err(|problem| {
            anyhow!(
                "Docker Compose ne lit pas {} : {problem}",
                self.file.display()
            )
        })
    }

    /// La forme résolue : variables remplacées, chemins absolus (docs/PROTOCOLE.md, « Compose »).
    pub fn resolved(&self) -> Result<Vec<u8>> {
        self.run(&mut self.command())
    }

    /// La forme brute, à publier : variables et chemins relatifs gardés, sans normalisation.
    pub fn source(&self) -> Result<Value> {
        let bytes = self.run(self.command().args([
            "--no-interpolate",
            "--no-path-resolution",
            "--no-normalize",
        ]))?;
        serde_json::from_slice(&bytes).context("sortie de docker compose config illisible")
    }
}

/// Construit une image pour ces plateformes, sans attestations, dans le dossier OCI `dest`. La
/// sortie de buildx s'affiche au fil de la construction.
pub fn build(spec: &BuildSpec, platforms: &[&str], dest: &Path) -> Result<()> {
    let mut command = docker();
    command
        .args(["buildx", "build", "--platform"])
        .arg(platforms.join(","))
        .args(["--provenance=false", "--sbom=false", "--output"])
        .arg(format!("type=oci,tar=false,dest={}", dest.display()))
        .args(spec.buildx_args())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if spec.dockerfile_inline.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(|error| match error.kind() {
        ErrorKind::NotFound => anyhow!(
            "{} introuvable : installe Docker (Docker Desktop, OrbStack…)",
            program().to_string_lossy()
        ),
        _ => anyhow!("docker buildx build : {error}"),
    })?;
    if let Some(inline) = &spec.dockerfile_inline
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(inline.as_bytes())
            .context("envoi du Dockerfile à docker buildx build")?;
    }
    let status = child.wait().context("docker buildx build")?;
    if !status.success() {
        bail!(
            "docker buildx build a échoué ({status}). Si le constructeur ne sait pas produire d'images OCI multi-plateformes, active le stockage d'images containerd de Docker, ou crée un constructeur : docker buildx create --use --driver docker-container"
        );
    }
    Ok(())
}
