//! Le compose d'une stack, tel que le lit Docker Compose : les images à construire, et le compose
//! publié (docs/PROTOCOLE.md, « Compose »). L'outil n'a pas de lecteur YAML : il travaille sur le
//! JSON que donne `docker compose config`, en deux lectures du même `compose.yaml` :
//!
//! - la forme **résolue** (variables remplacées, chemins absolus), pour savoir quoi construire et
//!   comment ;
//! - la forme **brute** (`--no-interpolate --no-path-resolution --no-normalize`), qui garde les
//!   `${VAR}` et les chemins relatifs. C'est elle qui est publiée, sans le `name` de haut niveau ni
//!   les sections `build`, chaque image construite remplacée par
//!   `<hôte>/<app>/<image>@<empreinte de son index>`. Le JSON est du YAML : le compose publié est
//!   ce JSON seul, sans commentaire.
//!
//! Une image construite prend le nom de l'`image:` de son service, sans registre ni étiquette
//! (`home-manager:dev` donne `home-manager`), ou à défaut le nom du service. Un service sans
//! `build` dont l'`image:` est celle d'un service construit emploie la même image.

use std::collections::BTreeMap;

use am_protocol::stack::{Digest, is_image_name};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};

/// Au plus 16 images par stack (`images` de `stack.json`).
pub const MAX_IMAGES: usize = 16;

/// Champs de `build` que l'outil sait passer à `docker buildx build`.
const BUILD_FIELDS: [&str; 14] = [
    "context",
    "dockerfile",
    "dockerfile_inline",
    "args",
    "target",
    "labels",
    "additional_contexts",
    "network",
    "no_cache",
    "pull",
    "extra_hosts",
    "cache_from",
    "cache_to",
    "shm_size",
];
/// Champs de `build` sans objet ici : les étiquettes ne sont pas publiées, et les plateformes sont
/// celles de `stack.json`.
const IGNORED_BUILD_FIELDS: [&str; 2] = ["tags", "platforms"];

/// Ce qu'il faut construire.
#[derive(Debug, Default)]
pub struct Plan {
    /// Triées par nom.
    pub images: Vec<PlannedImage>,
    /// Remarques qui n'empêchent pas la publication.
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct PlannedImage {
    /// Nom dans le registre du serveur, après `<app>/`.
    pub name: String,
    /// Services qui la construisent (`build:`).
    pub built_by: Vec<String>,
    /// Services qui l'emploient sans la construire, par la même `image:`.
    pub used_by: Vec<String>,
    pub build: BuildSpec,
    /// `image:` des services qui la construisent, dans la forme résolue.
    local_refs: Vec<String>,
}

impl PlannedImage {
    /// Tous les services qui l'emploient, triés.
    pub fn services(&self) -> Vec<&str> {
        let mut services: Vec<&str> = self
            .built_by
            .iter()
            .chain(&self.used_by)
            .map(String::as_str)
            .collect();
        services.sort_unstable();
        services
    }
}

impl Plan {
    /// Lit la forme résolue du compose : les services construits sur place, groupés par image.
    pub fn from_resolved(resolved: &Value) -> Result<Self> {
        let services = services(resolved)?;
        let mut groups: BTreeMap<String, PlannedImage> = BTreeMap::new();
        let mut notes = Vec::new();
        for name in sorted_keys(services) {
            let service = object(&services[name], &format!("service {name}"))?;
            let Some(build) = service.get("build").filter(|build| !build.is_null()) else {
                continue;
            };
            let image = service.get("image").and_then(Value::as_str);
            let image_name = image_name(name, image)?;
            let (spec, ignored) = BuildSpec::parse(name, build)?;
            for field in ignored {
                notes.push(format!(
                    "service {name} : build.{field} ignoré ({})",
                    if field == "platforms" {
                        "les plateformes sont celles de stack.json"
                    } else {
                        "rien n'est étiqueté, les images se désignent par empreinte"
                    }
                ));
            }
            match groups.get_mut(&image_name) {
                Some(group) => {
                    ensure!(
                        group.build == spec,
                        "les services {} et {name} construisent deux images différentes sous le même nom {image_name} : donne-leur des image: distinctes, ou la même section build",
                        group.built_by[0]
                    );
                    group.built_by.push(name.clone());
                    if let Some(image) = image
                        && !group.local_refs.iter().any(|known| known == image)
                    {
                        group.local_refs.push(image.to_owned());
                    }
                }
                None => {
                    groups.insert(
                        image_name.clone(),
                        PlannedImage {
                            name: image_name,
                            built_by: vec![name.clone()],
                            used_by: Vec::new(),
                            build: spec,
                            local_refs: image.map(str::to_owned).into_iter().collect(),
                        },
                    );
                }
            }
        }
        ensure!(
            !groups.is_empty(),
            "aucun service du compose ne se construit sur place (build:) : une stack publie au moins une image, construite par drakbase release"
        );
        ensure!(
            groups.len() <= MAX_IMAGES,
            "{} images à construire : une stack en a {MAX_IMAGES} au plus",
            groups.len()
        );
        for name in sorted_keys(services) {
            let service = object(&services[name], &format!("service {name}"))?;
            if service.get("build").is_some_and(|build| !build.is_null()) {
                continue;
            }
            let Some(image) = service.get("image").and_then(Value::as_str) else {
                continue;
            };
            if let Some(group) = groups
                .values_mut()
                .find(|group| group.local_refs.iter().any(|known| known == image))
            {
                group.used_by.push(name.clone());
            }
        }
        Ok(Self {
            images: groups.into_values().collect(),
            notes,
        })
    }
}

/// Le nom d'une image construite : celui de son `image:`, sans registre, étiquette ni empreinte,
/// sinon celui du service.
fn image_name(service: &str, image: Option<&str>) -> Result<String> {
    let candidate = match image {
        Some(reference) => {
            let named = reference.split('@').next().unwrap_or(reference);
            let last = named.rsplit('/').next().unwrap_or(named);
            last.split(':').next().unwrap_or(last).to_owned()
        }
        None => service.to_ascii_lowercase(),
    };
    ensure!(
        is_image_name(&candidate),
        "service {service} : « {candidate} » ne peut pas nommer une image (minuscules et chiffres, séparés par un seul . _ ou -, 64 caractères au plus) : donne-lui image: <nom>"
    );
    Ok(candidate)
}

/// La section `build` d'un service, dans la forme résolue, réduite à ce que l'outil passe à
/// `docker buildx build`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildSpec {
    /// Dossier absolu, ou adresse d'un dépôt.
    pub context: String,
    pub dockerfile: Option<String>,
    pub dockerfile_inline: Option<String>,
    /// `None` : la valeur vient de l'environnement de `docker buildx`.
    pub args: Vec<(String, Option<String>)>,
    pub target: Option<String>,
    pub labels: Vec<(String, String)>,
    pub additional_contexts: Vec<(String, String)>,
    pub network: Option<String>,
    pub no_cache: bool,
    pub pull: bool,
    /// `hôte=adresse`.
    pub extra_hosts: Vec<String>,
    pub cache_from: Vec<String>,
    pub cache_to: Vec<String>,
    pub shm_size: Option<String>,
}

impl BuildSpec {
    /// Rend aussi les champs ignorés.
    fn parse(service: &str, build: &Value) -> Result<(Self, Vec<&'static str>)> {
        let build = match build {
            Value::String(context) => {
                return Ok((
                    Self {
                        context: context.clone(),
                        ..Self::default()
                    },
                    Vec::new(),
                ));
            }
            other => object(other, &format!("service {service} : build"))?,
        };
        let mut ignored = Vec::new();
        for field in sorted_keys(build) {
            if let Some(known) = IGNORED_BUILD_FIELDS.iter().find(|known| **known == field) {
                ignored.push(*known);
            } else if !BUILD_FIELDS.contains(&field.as_str()) {
                bail!(
                    "service {service} : build.{field} n'est pas pris en charge par drakbase release (seulement {})",
                    BUILD_FIELDS.join(", ")
                );
            }
        }
        let text = |field: &str| -> Result<Option<String>> {
            match build.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(Value::Number(value)) => Ok(Some(value.to_string())),
                Some(_) => bail!("service {service} : build.{field} doit être un texte"),
            }
        };
        let flag = |field: &str| build.get(field).and_then(Value::as_bool).unwrap_or(false);
        let context =
            text("context")?.with_context(|| format!("service {service} : build sans context"))?;
        let spec = Self {
            context,
            dockerfile: text("dockerfile")?,
            dockerfile_inline: text("dockerfile_inline")?,
            args: pairs(service, build, "args")?,
            target: text("target")?,
            labels: pairs(service, build, "labels")?
                .into_iter()
                .map(|(key, value)| (key, value.unwrap_or_default()))
                .collect(),
            additional_contexts: pairs(service, build, "additional_contexts")?
                .into_iter()
                .map(|(key, value)| (key, value.unwrap_or_default()))
                .collect(),
            network: text("network")?,
            no_cache: flag("no_cache"),
            pull: flag("pull"),
            extra_hosts: extra_hosts(service, build.get("extra_hosts"))?,
            cache_from: strings(service, build, "cache_from")?,
            cache_to: strings(service, build, "cache_to")?,
            shm_size: text("shm_size")?,
        };
        Ok((spec, ignored))
    }

    /// Les arguments de `docker buildx build`, après les options de plateforme et de sortie, le
    /// dossier de contexte en dernier. Avec `dockerfile_inline`, le Dockerfile se lit sur l'entrée
    /// standard (`-f -`).
    pub fn buildx_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.dockerfile_inline.is_some() {
            args.extend(["-f".to_owned(), "-".to_owned()]);
        } else if let Some(dockerfile) = &self.dockerfile {
            let remote = self.context.contains("://") || self.context.starts_with("git@");
            let path = if dockerfile.starts_with('/') || remote {
                dockerfile.clone()
            } else {
                format!("{}/{dockerfile}", self.context.trim_end_matches('/'))
            };
            args.extend(["-f".to_owned(), path]);
        }
        if let Some(target) = &self.target {
            args.extend(["--target".to_owned(), target.clone()]);
        }
        for (name, value) in &self.args {
            let arg = match value {
                Some(value) => format!("{name}={value}"),
                None => name.clone(),
            };
            args.extend(["--build-arg".to_owned(), arg]);
        }
        for (name, value) in &self.labels {
            args.extend(["--label".to_owned(), format!("{name}={value}")]);
        }
        for (name, value) in &self.additional_contexts {
            args.extend(["--build-context".to_owned(), format!("{name}={value}")]);
        }
        if let Some(network) = &self.network {
            args.extend(["--network".to_owned(), network.clone()]);
        }
        if self.no_cache {
            args.push("--no-cache".to_owned());
        }
        if self.pull {
            args.push("--pull".to_owned());
        }
        for host in &self.extra_hosts {
            args.extend(["--add-host".to_owned(), host.clone()]);
        }
        for cache in &self.cache_from {
            args.extend(["--cache-from".to_owned(), cache.clone()]);
        }
        for cache in &self.cache_to {
            args.extend(["--cache-to".to_owned(), cache.clone()]);
        }
        if let Some(size) = &self.shm_size {
            args.extend(["--shm-size".to_owned(), size.clone()]);
        }
        args.push(self.context.clone());
        args
    }
}

/// Une table `{clé: valeur}`, ou une liste `clé=valeur` (`clé` seule : sans valeur).
fn pairs(
    service: &str,
    build: &Map<String, Value>,
    field: &str,
) -> Result<Vec<(String, Option<String>)>> {
    let scalar = |value: &Value| -> Result<Option<String>> {
        match value {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text.clone())),
            Value::Number(number) => Ok(Some(number.to_string())),
            Value::Bool(flag) => Ok(Some(flag.to_string())),
            _ => bail!("service {service} : build.{field} : valeurs simples attendues"),
        }
    };
    match build.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => sorted_keys(map)
            .into_iter()
            .map(|key| Ok((key.clone(), scalar(&map[key])?)))
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let text = item.as_str().with_context(|| {
                    format!("service {service} : build.{field} : textes attendus")
                })?;
                Ok(match text.split_once('=') {
                    Some((key, value)) => (key.to_owned(), Some(value.to_owned())),
                    None => (text.to_owned(), None),
                })
            })
            .collect(),
        Some(_) => bail!("service {service} : build.{field} : table ou liste attendue"),
    }
}

fn strings(service: &str, build: &Map<String, Value>, field: &str) -> Result<Vec<String>> {
    match build.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("service {service} : build.{field} : textes attendus"))
            })
            .collect(),
        Some(Value::String(text)) => Ok(vec![text.clone()]),
        Some(_) => bail!("service {service} : build.{field} : liste attendue"),
    }
}

/// `extra_hosts` : `{hôte: [adresses]}` (Compose récent), `{hôte: adresse}`, ou une liste
/// `hôte:adresse` / `hôte=adresse`. Rend des `hôte=adresse`.
fn extra_hosts(service: &str, value: Option<&Value>) -> Result<Vec<String>> {
    let invalid = || anyhow::anyhow!("service {service} : build.extra_hosts illisible");
    let mut hosts = Vec::new();
    match value {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for host in sorted_keys(map) {
                match &map[host] {
                    Value::String(address) => hosts.push(format!("{host}={address}")),
                    Value::Array(addresses) => {
                        for address in addresses {
                            hosts.push(format!("{host}={}", address.as_str().ok_or_else(invalid)?));
                        }
                    }
                    _ => return Err(invalid()),
                }
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                let text = item.as_str().ok_or_else(invalid)?;
                let (host, address) = text
                    .split_once('=')
                    .or_else(|| text.split_once(':'))
                    .ok_or_else(invalid)?;
                hosts.push(format!("{host}={address}"));
            }
        }
        Some(_) => return Err(invalid()),
    }
    Ok(hosts)
}

/// Le compose publié, à partir de la forme brute : sans `name` ni `build`, chaque image construite
/// remplacée par `<prefix>/<image>@<empreinte>` (`prefix` : `<hôte>/<app>`). Le texte est ce JSON
/// seul : l'agent refuse tout ce qui n'est pas exactement un document JSON, commentaire compris
/// (docs/PROTOCOLE.md, « Compose de l'archive »).
pub fn publish(
    source: &Value,
    plan: &Plan,
    prefix: &str,
    digests: &BTreeMap<String, Digest>,
) -> Result<String> {
    check_volume_sources(source)?;
    let mut published = source.clone();
    let root = published
        .as_object_mut()
        .context("configuration Compose : objet attendu")?;
    root.remove("name");
    let services = root
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .context("configuration Compose sans services")?;
    for image in &plan.images {
        let digest = digests
            .get(&image.name)
            .with_context(|| format!("empreinte de l'image {} inconnue", image.name))?;
        let reference = format!("{prefix}/{}@{digest}", image.name);
        for name in image.built_by.iter().chain(&image.used_by) {
            let service = services
                .get_mut(name)
                .and_then(Value::as_object_mut)
                .with_context(|| format!("service {name} absent de la forme brute du compose"))?;
            service.remove("build");
            // Une image publiée se tire du serveur : « build » ou « never » l'en empêcheraient.
            if matches!(
                service.get("pull_policy").and_then(Value::as_str),
                Some("build" | "never")
            ) {
                service.remove("pull_policy");
            }
            service.insert("image".to_owned(), Value::String(reference.clone()));
        }
    }
    if let Some((name, _)) = services
        .iter()
        .find(|(_, service)| service.get("build").is_some_and(|build| !build.is_null()))
    {
        bail!("service {name} : section build restée dans le compose publié");
    }
    let mut text = serde_json::to_string_pretty(&published)?;
    text.push('\n');
    Ok(text)
}

/// Sans lire les variables, Docker Compose ne sait pas si `${DOSSIER}:/data` désigne un dossier ou
/// un volume : il en fait un volume, qui ne vaudrait plus rien une fois la variable lue. Un tel
/// montage s'écrit donc en syntaxe longue, avec son type.
fn check_volume_sources(source: &Value) -> Result<()> {
    let services = services(source)?;
    for name in sorted_keys(services) {
        let Some(volumes) = services[name].get("volumes").and_then(Value::as_array) else {
            continue;
        };
        for volume in volumes {
            let Some(volume) = volume.as_object() else {
                continue;
            };
            let source = volume.get("source").and_then(Value::as_str).unwrap_or("");
            if volume.get("type").and_then(Value::as_str) == Some("volume") && source.contains('$')
            {
                let target = volume.get("target").and_then(Value::as_str).unwrap_or("?");
                bail!(
                    "service {name} : le montage « {source}:{target} » prend une variable pour source. Docker Compose ne peut pas savoir, avant de la lire, s'il s'agit d'un dossier ou d'un volume : écris-le en syntaxe longue, par exemple {{ type: bind, source: {source}, target: {target} }}"
                );
            }
        }
    }
    Ok(())
}

fn services(config: &Value) -> Result<&Map<String, Value>> {
    config
        .get("services")
        .and_then(Value::as_object)
        .context("configuration Compose sans services")
}

fn object<'v>(value: &'v Value, what: &str) -> Result<&'v Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{what} : objet attendu"))
}

/// Clés triées, quel que soit l'ordre que garde serde_json.
fn sorted_keys(map: &Map<String, Value>) -> Vec<&String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn digest(name: &str) -> Digest {
        Digest::of(name.as_bytes())
    }

    /// Forme résolue d'une stack : `web` et `backup` construisent la même image, `worker` emploie
    /// celle de `web` sans la construire, `redis` vient de Docker Hub.
    fn resolved() -> Value {
        json!({
            "name": "demo",
            "services": {
                "web": {
                    "build": {"context": "/projet/app", "dockerfile": "Dockerfile", "args": {"MODE": "prod", "TOKEN": null}},
                    "image": "registry.local:5000/equipe/demo-web:dev"
                },
                "backup": {
                    "build": {"context": "/projet/app", "dockerfile": "Dockerfile", "args": {"MODE": "prod", "TOKEN": null}},
                    "image": "demo-web:dev",
                    "profiles": ["drakbase-hooks"]
                },
                "worker": {"image": "demo-web:dev"},
                "jobs": {"build": {"context": "/projet/jobs", "dockerfile_inline": "FROM scratch\n", "platforms": ["linux/arm64"]}},
                "redis": {"image": "redis:7@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            }
        })
    }

    #[test]
    fn built_services_are_grouped_by_image() {
        let plan = Plan::from_resolved(&resolved()).unwrap();
        let names: Vec<&str> = plan
            .images
            .iter()
            .map(|image| image.name.as_str())
            .collect();
        assert_eq!(names, ["demo-web", "jobs"]);
        let web = &plan.images[0];
        assert_eq!(web.built_by, ["backup", "web"]);
        assert_eq!(web.used_by, ["worker"]);
        assert_eq!(web.services(), ["backup", "web", "worker"]);
        assert_eq!(
            web.build.args,
            [
                ("MODE".to_owned(), Some("prod".to_owned())),
                ("TOKEN".to_owned(), None)
            ]
        );
        assert_eq!(
            plan.notes,
            ["service jobs : build.platforms ignoré (les plateformes sont celles de stack.json)"]
        );
    }

    #[test]
    fn plans_refuse_what_cannot_be_published() {
        let mut two_builds = resolved();
        two_builds["services"]["backup"]["build"]["target"] = json!("backup");
        let error = Plan::from_resolved(&two_builds).unwrap_err().to_string();
        assert!(
            error.contains("deux images différentes sous le même nom demo-web"),
            "{error}"
        );

        let mut bad_name = resolved();
        bad_name["services"]["jobs"]["image"] = json!("Jobs_Runner:1");
        let error = Plan::from_resolved(&bad_name).unwrap_err().to_string();
        assert!(
            error.contains("« Jobs_Runner » ne peut pas nommer une image"),
            "{error}"
        );

        let mut secrets = resolved();
        secrets["services"]["jobs"]["build"]["secrets"] = json!(["token"]);
        let error = Plan::from_resolved(&secrets).unwrap_err().to_string();
        assert!(
            error.contains("build.secrets n'est pas pris en charge"),
            "{error}"
        );

        let nothing = json!({"services": {"redis": {"image": "redis:7"}}});
        let error = Plan::from_resolved(&nothing).unwrap_err().to_string();
        assert!(
            error.contains("aucun service du compose ne se construit"),
            "{error}"
        );

        let many: Map<String, Value> = (0..17)
            .map(|index| (format!("s{index}"), json!({"build": {"context": "/x"}})))
            .collect();
        let error = Plan::from_resolved(&json!({"services": many}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("17 images à construire"), "{error}");
    }

    #[test]
    fn image_names_come_from_the_image_or_the_service() {
        assert_eq!(image_name("web", Some("demo-web:dev")).unwrap(), "demo-web");
        assert_eq!(
            image_name("web", Some("localhost:5000/a/b/web-ui:1.2@sha256:00")).unwrap(),
            "web-ui"
        );
        assert_eq!(image_name("Home_Manager", None).unwrap(), "home_manager");
        assert!(image_name("web", Some("demo--web")).is_err());
        assert!(image_name("-web", None).is_err());
    }

    #[test]
    fn buildx_gets_the_build_section() {
        let (spec, ignored) = BuildSpec::parse(
            "web",
            &json!({
                "context": "/projet/app",
                "dockerfile": "docker/Dockerfile",
                "target": "runtime",
                "args": {"MODE": "prod", "TOKEN": null},
                "labels": {"org.example": "1"},
                "additional_contexts": {"assets": "/projet/assets"},
                "network": "host",
                "no_cache": true,
                "pull": true,
                "extra_hosts": {"db": ["10.0.0.2", "::1"]},
                "cache_from": ["type=local,src=/tmp/cache"],
                "shm_size": 1_073_741_824u64,
                "tags": ["demo:1"]
            }),
        )
        .unwrap();
        assert_eq!(ignored, ["tags"]);
        assert_eq!(
            spec.buildx_args(),
            [
                "-f",
                "/projet/app/docker/Dockerfile",
                "--target",
                "runtime",
                "--build-arg",
                "MODE=prod",
                "--build-arg",
                "TOKEN",
                "--label",
                "org.example=1",
                "--build-context",
                "assets=/projet/assets",
                "--network",
                "host",
                "--no-cache",
                "--pull",
                "--add-host",
                "db=10.0.0.2",
                "--add-host",
                "db=::1",
                "--cache-from",
                "type=local,src=/tmp/cache",
                "--shm-size",
                "1073741824",
                "/projet/app"
            ]
        );
        let (inline, _) = BuildSpec::parse(
            "jobs",
            &json!({"context": "/projet/jobs", "dockerfile_inline": "FROM scratch\n", "extra_hosts": ["a:1.2.3.4"]}),
        )
        .unwrap();
        assert_eq!(
            inline.buildx_args(),
            ["-f", "-", "--add-host", "a=1.2.3.4", "/projet/jobs"]
        );
        let (remote, _) = BuildSpec::parse(
            "web",
            &json!({"context": "https://example.fr/depot.git#main", "dockerfile": "Dockerfile"}),
        )
        .unwrap();
        assert_eq!(
            remote.buildx_args(),
            ["-f", "Dockerfile", "https://example.fr/depot.git#main"]
        );
    }

    /// Forme brute du même compose : variables et chemins relatifs gardés.
    fn source() -> Value {
        json!({
            "name": "demo",
            "services": {
                "web": {
                    "build": {"context": "../app", "dockerfile": "Dockerfile"},
                    "image": "registry.local:5000/equipe/demo-web:dev",
                    "pull_policy": "build",
                    "ports": ["${WEB_PORT:-8080}:80"],
                    "environment": {"TZ": "${TZ:-Europe/Paris}", "MSG": "a $$ dollar"},
                    "volumes": [
                        {"type": "bind", "source": "./files/config.toml", "target": "/etc/config.toml", "read_only": true},
                        {"type": "bind", "source": "${DATA_DIR}", "target": "/srv"},
                        {"type": "volume", "source": "data", "target": "/data", "volume": {}}
                    ]
                },
                "backup": {"build": {"context": "../app"}, "image": "demo-web:dev", "profiles": ["drakbase-hooks"]},
                "worker": {"image": "demo-web:dev", "pull_policy": "always"},
                "jobs": {"build": {"context": "../jobs", "dockerfile_inline": "FROM scratch\n"}},
                "redis": {"image": "redis:7@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
            },
            "volumes": {"data": null}
        })
    }

    #[test]
    fn the_published_compose_pins_the_built_images() {
        let plan = Plan::from_resolved(&resolved()).unwrap();
        let digests = BTreeMap::from([
            ("demo-web".to_owned(), digest("demo-web")),
            ("jobs".to_owned(), digest("jobs")),
        ]);
        let text = publish(&source(), &plan, "maj.example.fr/demo", &digests).unwrap();
        // Du JSON seul, sans commentaire : ce que l'agent admet.
        assert!(text.starts_with('{') && text.ends_with("}\n"), "{text}");
        am_protocol::stack::compose::check_archived(text.as_bytes()).unwrap();
        let published: Value = serde_json::from_str(&text).unwrap();
        let web = format!("maj.example.fr/demo/demo-web@{}", digest("demo-web"));
        let jobs = format!("maj.example.fr/demo/jobs@{}", digest("jobs"));
        assert_eq!(
            published,
            json!({
                "services": {
                    "web": {
                        "image": web,
                        "ports": ["${WEB_PORT:-8080}:80"],
                        "environment": {"TZ": "${TZ:-Europe/Paris}", "MSG": "a $$ dollar"},
                        "volumes": [
                            {"type": "bind", "source": "./files/config.toml", "target": "/etc/config.toml", "read_only": true},
                            {"type": "bind", "source": "${DATA_DIR}", "target": "/srv"},
                            {"type": "volume", "source": "data", "target": "/data", "volume": {}}
                        ]
                    },
                    "backup": {"image": web, "profiles": ["drakbase-hooks"]},
                    "worker": {"image": web, "pull_policy": "always"},
                    "jobs": {"image": jobs},
                    "redis": {"image": "redis:7@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
                },
                "volumes": {"data": null}
            })
        );
        // Mêmes entrées, mêmes octets.
        assert_eq!(
            publish(&source(), &plan, "maj.example.fr/demo", &digests).unwrap(),
            text
        );

        let missing = BTreeMap::from([("demo-web".to_owned(), digest("demo-web"))]);
        let error = publish(&source(), &plan, "maj.example.fr/demo", &missing)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("empreinte de l'image jobs inconnue"),
            "{error}"
        );
    }

    #[test]
    fn a_volume_named_by_a_variable_must_say_its_type() {
        let plan = Plan::from_resolved(&resolved()).unwrap();
        let digests = BTreeMap::from([
            ("demo-web".to_owned(), digest("demo-web")),
            ("jobs".to_owned(), digest("jobs")),
        ]);
        let mut source = source();
        // `${DATA_DIR}:/srv` en syntaxe courte, lu sans les variables.
        source["services"]["web"]["volumes"][1] = json!({"type": "volume", "source": "${DATA_DIR:-/srv/demo}", "target": "/srv", "volume": {}});
        let error = publish(&source, &plan, "maj.example.fr/demo", &digests)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("service web : le montage « ${DATA_DIR:-/srv/demo}:/srv » prend une variable pour source"),
            "{error}"
        );
        assert!(error.contains("type: bind"), "{error}");
    }
}
