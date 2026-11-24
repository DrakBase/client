//! Stacks Docker (docs/PLAN.md, « Stacks Docker », et docs/PROTOCOLE.md, section du même nom) :
//! le dossier `stack/` du projet, son `stack.json`, la lecture de son compose par Docker Compose,
//! les images construites et l'archive publiée.
//!
//! Le dossier de la stack contient :
//! - `compose.yaml`, un compose ordinaire, où les services construits sur place ont un `build:` ;
//! - `stack.json`, dans sa lecture « source » : service surveillé, retour arrière, plateformes,
//!   clés publiques (que `drakbase init` y écrit) ;
//! - `.env.example`, facultatif : les réglages de la machine et leur valeur par défaut ;
//! - `files/`, facultatif : des fichiers que le compose monte en lecture seule.

pub mod compose;
pub mod docker;
pub mod images;
pub mod release;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::stack::archive::{COMPOSE_YAML, ENV_EXAMPLE, FILES_DIR, STACK_JSON};
use am_protocol::stack::compose::{
    ComposeContext, ComposeReport, analyze, check_archived, check_env_example,
};
use am_protocol::stack::manifest::StackManifest;
use am_protocol::stack::{Digest, StackError};
use am_protocol::{Arch, Channel, PRODUCTION_SERVER, ServerUrl, Version};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;

use crate::home::AppSlug;
use crate::project;

use self::compose::Plan;
use self::docker::ComposeProject;

/// Dossier de la stack par défaut, à la racine du projet.
pub const DEFAULT_DIR: &str = "stack";

/// Le dossier d'une stack.
#[derive(Clone, Debug)]
pub struct StackProject {
    pub dir: PathBuf,
}

impl StackProject {
    /// Le dossier `dir`, qui doit contenir `compose.yaml`.
    pub fn at(dir: &Path) -> Result<Self> {
        let project = Self {
            dir: dir.to_owned(),
        };
        if !project.compose().is_file() {
            let other = ["compose.yml", "docker-compose.yaml", "docker-compose.yml"]
                .iter()
                .find(|name| dir.join(name).is_file());
            match other {
                Some(name) => bail!(
                    "{} : renomme {name} en {COMPOSE_YAML}, le seul nom que lisent l'outil et l'agent",
                    project::display(dir)
                ),
                None => bail!(
                    "{} introuvable : le dossier d'une stack contient son compose.yaml (et stack.json, .env.example, files/)",
                    project::display(&project.compose())
                ),
            }
        }
        Ok(project)
    }

    /// Le dossier `stack/` de `root`, s'il a un `compose.yaml`.
    pub fn find(root: &Path) -> Option<Self> {
        let dir = root.join(DEFAULT_DIR);
        dir.join(COMPOSE_YAML).is_file().then_some(Self { dir })
    }

    pub fn compose(&self) -> PathBuf {
        self.dir.join(COMPOSE_YAML)
    }

    pub fn stack_json(&self) -> PathBuf {
        self.dir.join(STACK_JSON)
    }

    pub fn env_example(&self) -> PathBuf {
        self.dir.join(ENV_EXAMPLE)
    }

    /// `.env.example`, s'il existe.
    pub fn env_file(&self) -> Option<PathBuf> {
        Some(self.env_example()).filter(|path| path.is_file())
    }

    /// Le dossier des sorties par défaut : `dist/`, à côté du dossier de la stack.
    pub fn default_output(&self) -> PathBuf {
        self.dir
            .parent()
            .map_or_else(|| PathBuf::from("dist"), |parent| parent.join("dist"))
    }

    /// Lit `stack.json` dans sa lecture « source ».
    pub fn manifest(&self) -> Result<StackManifest> {
        let path = self.stack_json();
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "{} : drakbase init <app> le crée, avec les clés publiques",
                project::display(&path)
            )
        })?;
        StackManifest::parse_source(&bytes).map_err(|error| describe(&path, &error))
    }

    /// Le projet Compose, lu avec `.env.example` comme fichier de variables : c'est de lui que la
    /// machine tirera son `.env`.
    pub fn compose_project(&self, app: &str) -> ComposeProject {
        ComposeProject {
            name: app.to_owned(),
            dir: self.dir.clone(),
            file: self.compose(),
            env_file: self.env_file(),
        }
    }

    /// Les fichiers de l'archive, sauf `stack.json` et `compose.yaml` : `.env.example` et
    /// `files/`, par chemin. Les liens sont refusés ; `.DS_Store` et les fichiers AppleDouble
    /// (`._*`) sont laissés de côté.
    pub fn extra_files(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut files = BTreeMap::new();
        if let Some(env) = self.env_file() {
            let contents =
                fs::read(&env).with_context(|| format!("lecture de {}", env.display()))?;
            // Comme le fera l'agent : une stack ne choisit pas les réglages de Docker Compose.
            check_env_example(&contents).map_err(|error| {
                anyhow!("{} : {error} ({})", project::display(&env), error.code())
            })?;
            files.insert(ENV_EXAMPLE.to_owned(), contents);
        }
        let dir = self.dir.join(FILES_DIR);
        if dir.is_dir() {
            collect(&dir, FILES_DIR, &mut files)?;
        }
        Ok(files)
    }
}

/// Une règle de stack enfreinte, avec le fichier et son code.
fn describe(path: &Path, error: &StackError) -> anyhow::Error {
    anyhow!("{} : {error} ({})", project::display(path), error.code())
}

fn collect(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("lecture de {}", dir.display()))?
        .collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name
            .to_str()
            .with_context(|| format!("{} : nom de fichier hors UTF-8", entry.path().display()))?;
        if name == ".DS_Store" || name.starts_with("._") {
            continue;
        }
        let path = entry.path();
        let relative = format!("{prefix}/{name}");
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "{} est un lien : une stack ne contient que des fichiers et des dossiers",
                path.display()
            );
        } else if kind.is_dir() {
            collect(&path, &relative, files)?;
        } else {
            files.insert(
                relative,
                fs::read(&path).with_context(|| format!("lecture de {}", path.display()))?,
            );
        }
    }
    Ok(())
}

/// `<app>-<version>-<arch>.stack.tar.gz` : le nom de l'archive, qui entre dans le commentaire de
/// confiance de sa signature.
pub fn archive_name(app: &AppSlug, version: &Version, arch: Arch) -> String {
    format!("{app}-{version}-{arch}.stack.tar.gz")
}

/// La commande d'installation à transmettre, clé publique comprise : sur la machine, d'abord
/// l'agent, puis la stack.
pub fn install_commands(
    server: &ServerUrl,
    app: &AppSlug,
    public_key: &str,
    channel: Channel,
) -> [String; 2] {
    let mut install = format!("sudo drakbase-stack install {app} --key {public_key}");
    // Le serveur de production est celui de l'agent par défaut : la commande ne le précise pas.
    if server.as_str() != PRODUCTION_SERVER {
        install.push_str(&format!(" --server {server}"));
    }
    if channel == Channel::Beta {
        install.push_str(" --channel beta");
    }
    [
        format!("curl -fsSL {server}/install/drakbase-stack.sh | sudo sh"),
        install,
    ]
}

/// Ce que Docker Compose dit de la stack, avant ou après la construction des images.
pub struct Examination {
    pub plan: Plan,
    /// Le compose publié : à mettre tel quel dans l'archive.
    pub published: String,
    pub report: ComposeReport,
}

/// Lit le compose avec Docker Compose (forme brute et forme résolue), en tire les images à
/// construire, écrit le compose publié avec ces empreintes (des empreintes provisoires, tirées du
/// nom des images, avant la construction), puis le contrôle comme le fera l'agent : sa forme
/// résolue, dans un dossier qui ne contient que ce que contiendra l'archive, avec `.env.example`
/// comme fichier de variables.
pub fn examine(
    project: &StackProject,
    manifest: &StackManifest,
    app: &AppSlug,
    host: &str,
    digests: Option<&BTreeMap<String, Digest>>,
) -> Result<Examination> {
    let compose_project = project.compose_project(&app.to_string());
    let source = compose_project.source()?;
    let resolved: Value = serde_json::from_slice(&compose_project.resolved()?)
        .context("sortie de docker compose config illisible")?;
    let plan = Plan::from_resolved(&resolved)?;
    let provisional: BTreeMap<String, Digest>;
    let digests = match digests {
        Some(digests) => digests,
        None => {
            provisional = plan
                .images
                .iter()
                .map(|image| (image.name.clone(), Digest::of(image.name.as_bytes())))
                .collect();
            &provisional
        }
    };
    let published = compose::publish(&source, &plan, &format!("{host}/{app}"), digests)?;
    // Tel qu'il est écrit d'abord, puis dans sa forme résolue : les deux contrôles de l'agent.
    check_archived(published.as_bytes()).map_err(|error| {
        anyhow!(
            "{} : {error} ({})",
            project::display(&project.compose()),
            error.code()
        )
    })?;
    let report = check_published(project, manifest, app, host, &published, digests)?;
    Ok(Examination {
        plan,
        published,
        report,
    })
}

/// Le contrôle du compose publié, dans un dossier factice qui ne contient que ce que contiendra
/// l'archive.
fn check_published(
    project: &StackProject,
    manifest: &StackManifest,
    app: &AppSlug,
    host: &str,
    published: &str,
    digests: &BTreeMap<String, Digest>,
) -> Result<ComposeReport> {
    let temp = tempfile::Builder::new()
        .prefix("drakbase-stack-")
        .tempdir()
        .context("dossier temporaire")?;
    // Le chemin réel : Docker Compose écrit les chemins absolus à partir de celui-ci.
    let dir = fs::canonicalize(temp.path())?;
    let mut files = project.extra_files()?;
    files.insert(COMPOSE_YAML.to_owned(), published.as_bytes().to_vec());
    for (path, contents) in &files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, contents)?;
    }
    let env_file = dir.join(ENV_EXAMPLE);
    let slug = app.to_string();
    let resolved = ComposeProject {
        name: slug.clone(),
        dir: dir.clone(),
        file: dir.join(COMPOSE_YAML),
        env_file: env_file.is_file().then_some(env_file),
    }
    .resolved()
    .context("compose publié")?;
    let dir_text = dir
        .to_str()
        .context("dossier temporaire hors UTF-8")?
        .trim_end_matches('/')
        .to_owned();
    analyze(
        &resolved,
        &ComposeContext {
            app: &slug,
            host,
            project_dir: &dir_text,
            service: &manifest.service,
            pre_update: manifest.pre_update.as_deref(),
            images: digests,
        },
    )
    .map_err(|error| {
        anyhow!(
            "{} : {error} ({})",
            project::display(&project.compose()),
            error.code()
        )
    })
}

/// Le `stack.json` publié et l'archive : `stack.json`, le compose publié, `.env.example` et
/// `files/`, écrits de façon reproductible.
pub fn archive(
    project: &StackProject,
    manifest: &StackManifest,
    app: &AppSlug,
    version: &Version,
    digests: BTreeMap<String, Digest>,
    published_compose: &str,
) -> Result<Vec<u8>> {
    let published = manifest
        .clone()
        .published(&app.to_string(), version, digests);
    let stack_json = published.to_json();
    // Relu comme le relira le serveur.
    StackManifest::parse_published(stack_json.as_bytes(), &app.to_string(), version)
        .map_err(|error| anyhow!("stack.json publié : {error} ({})", error.code()))?;
    let mut files = project.extra_files()?;
    files.insert(STACK_JSON.to_owned(), stack_json.into_bytes());
    files.insert(
        COMPOSE_YAML.to_owned(),
        published_compose.as_bytes().to_vec(),
    );
    let bytes = am_protocol::stack::archive::write(&files)
        .map_err(|error| anyhow!("archive de la stack : {error} ({})", error.code()))?;
    ensure!(
        am_protocol::stack::archive::read(&bytes).is_ok(),
        "archive de la stack illisible une fois écrite"
    );
    Ok(bytes)
}

/// « linux/arm64 et linux/amd64 ».
pub fn platform_list(archs: &[Arch]) -> String {
    let names: Vec<&str> = archs
        .iter()
        .filter_map(|arch| am_protocol::stack::docker_platform(*arch))
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} et {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_contract() {
        let app: AppSlug = "home-manager".parse().unwrap();
        let version = Version::parse_release("0.2").unwrap();
        assert_eq!(
            archive_name(&app, &version, Arch::Aarch64),
            "home-manager-0.2.0-aarch64.stack.tar.gz"
        );
        assert_eq!(
            platform_list(&[Arch::Aarch64, Arch::X86_64, Arch::Armv7]),
            "linux/arm64, linux/amd64 et linux/arm/v7"
        );
    }

    #[test]
    fn the_install_command_carries_the_key() {
        let app: AppSlug = "home-manager".parse().unwrap();
        let production = ServerUrl::parse(PRODUCTION_SERVER).unwrap();
        assert_eq!(
            install_commands(&production, &app, "Q0xF", Channel::Stable),
            [
                "curl -fsSL https://updates.drakbase.com/install/drakbase-stack.sh | sudo sh",
                "sudo drakbase-stack install home-manager --key Q0xF"
            ]
        );
        let other = ServerUrl::parse("https://maj.example.fr").unwrap();
        assert_eq!(
            install_commands(&other, &app, "Q0xF", Channel::Beta)[1],
            "sudo drakbase-stack install home-manager --key Q0xF --server https://maj.example.fr --channel beta"
        );
    }

    #[test]
    fn the_archive_takes_the_stack_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let stack = dir.path().join("stack");
        fs::create_dir_all(stack.join("files/conf")).unwrap();
        fs::write(stack.join("compose.yaml"), "services: {}\n").unwrap();
        fs::write(stack.join(".env.example"), "PORT=8080\n").unwrap();
        fs::write(stack.join(".env"), "PORT=9999\n").unwrap();
        fs::write(stack.join("notes.md"), "brouillon\n").unwrap();
        fs::write(stack.join("files/conf/app.toml"), "a = 1\n").unwrap();
        fs::write(stack.join("files/.DS_Store"), "").unwrap();
        fs::write(stack.join("files/._app.toml"), "").unwrap();
        let project = StackProject::at(&stack).unwrap();
        let files = project.extra_files().unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            [".env.example", "files/conf/app.toml"]
        );
        assert_eq!(project.default_output(), dir.path().join("dist"));

        std::os::unix::fs::symlink("/etc/hosts", stack.join("files/hosts")).unwrap();
        let error = project.extra_files().unwrap_err().to_string();
        assert!(error.contains("est un lien"), "{error}");
        fs::remove_file(stack.join("files/hosts")).unwrap();

        // Un réglage de Docker Compose n'est pas un réglage de la stack.
        fs::write(stack.join(".env.example"), "COMPOSE_PROFILES=x\n").unwrap();
        let error = project.extra_files().unwrap_err().to_string();
        assert!(error.contains("env_name_reserved"), "{error}");
        // Ni une forme que l'agent ne lirait pas comme Docker Compose.
        fs::write(stack.join(".env.example"), "PORT=8080\nTZ: Europe/Paris\n").unwrap();
        let error = project.extra_files().unwrap_err().to_string();
        assert!(error.contains(".env.example, ligne 2"), "{error}");
        assert!(error.contains("env_line_unsupported"), "{error}");
        fs::write(stack.join(".env.example"), "PORT=8080\n").unwrap();

        fs::rename(stack.join("compose.yaml"), stack.join("docker-compose.yml")).unwrap();
        let error = StackProject::at(&stack).unwrap_err().to_string();
        assert!(
            error.contains("renomme docker-compose.yml en compose.yaml"),
            "{error}"
        );
        assert!(StackProject::find(dir.path()).is_none());
    }
}
