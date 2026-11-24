//! Analyse du compose résolu par Docker Compose (`docker compose … --profile '*' config --format
//! json`) : règles d'une stack et permissions à faire accepter (docs/PROTOCOLE.md, « Compose »).
//! Et contrôle du `compose.yaml` d'une archive tel qu'il est écrit, avant que Compose ne le lise
//! (`check_archived`).
//!
//! La forme résolue ne contient que des champs d'une liste blanche : un champ que cette version
//! ne connaît pas est refusé, plutôt que de laisser passer sans la voir une façon nouvelle
//! d'atteindre la machine.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use super::archive::FILES_DIR;
use super::{Digest, HOOKS_PROFILE, StackError, is_app_slug, is_image_name};

/// Sections admises au premier niveau.
const TOP_LEVEL_FIELDS: &[&str] = &[
    "name", "version", "services", "volumes", "networks", "configs", "secrets",
];

/// Champs d'un service admis sans contrôle. Les `labels` en sont, bien que des outils de la
/// machine les lisent : ce qu'ils en font ne dépend pas de la stack.
const PLAIN_SERVICE_FIELDS: &[&str] = &[
    "attach",
    "cap_drop",
    "command",
    "configs",
    "cpu_count",
    "cpu_percent",
    "cpu_period",
    "cpu_quota",
    "cpu_rt_period",
    "cpu_shares",
    "cpus",
    "cpuset",
    "depends_on",
    "develop",
    "dns",
    "dns_opt",
    "dns_search",
    "domainname",
    "entrypoint",
    "environment",
    "expose",
    "extra_hosts",
    "group_add",
    "healthcheck",
    "hostname",
    "init",
    "labels",
    "links",
    "mac_address",
    "mem_limit",
    "mem_reservation",
    "mem_swappiness",
    "memswap_limit",
    "networks",
    "pids_limit",
    "platform",
    "pull_policy",
    "read_only",
    "restart",
    "scale",
    "secrets",
    "shm_size",
    "stdin_open",
    "stop_grace_period",
    "stop_signal",
    "storage_opt",
    // Le noyau n'accepte dans un conteneur que les réglages de ses propres espaces de noms.
    "sysctls",
    "tmpfs",
    "tty",
    "ulimits",
    "user",
    "working_dir",
];

/// Champs d'un service que contrôlent les règles ou la table des permissions.
const CHECKED_SERVICE_FIELDS: &[&str] = &[
    "image",
    "build",
    "profiles",
    "container_name",
    "volumes",
    "volumes_from",
    "env_file",
    "label_file",
    "network_mode",
    "pid",
    "ipc",
    "uts",
    "userns_mode",
    "cgroup",
    "privileged",
    "cap_add",
    "devices",
    "device_cgroup_rules",
    "security_opt",
    "ports",
    "gpus",
    "runtime",
    "deploy",
    "post_start",
    "pre_stop",
    "logging",
    "blkio_config",
    "cpu_rt_runtime",
    "oom_kill_disable",
    "oom_score_adj",
];

/// Champs de `blkio_config`. Ceux qui nomment un disque de la machine ne sont admis que vides.
const BLKIO_FIELDS: &[&str] = &[
    "weight",
    "weight_device",
    "device_read_bps",
    "device_write_bps",
    "device_read_iops",
    "device_write_iops",
];

/// Pilotes de journal qui écrivent sur la machine, sans que le service Docker se connecte nulle
/// part.
const LOGGING_DRIVERS: &[&str] = &["json-file", "local", "none"];

/// Champs de `logging`.
const LOGGING_FIELDS: &[&str] = &["driver", "options"];

/// Options de journal admises sans pilote explicite : c'est alors le pilote par défaut de la
/// machine qui les reçoit, et il peut envoyer le journal ailleurs (`syslog-address`…).
const DEFAULT_LOGGING_OPTIONS: &[&str] = &["max-size", "max-file", "compress"];

/// Champs d'un montage de service (`volumes` d'un service, forme longue).
const MOUNT_FIELDS: &[&str] = &[
    "type",
    "source",
    "target",
    "read_only",
    "consistency",
    "bind",
    "volume",
    "tmpfs",
];

/// Champs de l'objet `bind` d'un montage.
const MOUNT_BIND_FIELDS: &[&str] = &["propagation", "create_host_path", "selinux", "recursive"];

/// Champs de l'objet `volume` d'un montage.
const MOUNT_VOLUME_FIELDS: &[&str] = &["nocopy", "subpath", "labels"];

/// Champs de l'objet `tmpfs` d'un montage.
const MOUNT_TMPFS_FIELDS: &[&str] = &["size", "mode"];

/// Propagations d'un montage `bind` admises : aucune ne fait remonter vers la machine ce que le
/// conteneur monte. `shared` et `rshared` le feraient.
const BIND_PROPAGATIONS: &[&str] = &["private", "rprivate", "slave", "rslave"];

/// Valeurs de `bind.recursive` qui gardent en lecture seule un montage `read_only`, sous-montages
/// compris ou absents. `writable` laisse ses sous-montages en écriture.
const READ_ONLY_RECURSIVE: &[&str] = &["enabled", "disabled", "readonly"];

/// Options d'un volume du pilote `local`, qui disent ce que Docker monte.
const VOLUME_DRIVER_OPTS: &[&str] = &["type", "o", "device"];

/// Options de montage d'un volume `none` : le dossier de la machine, en lecture seule ou non,
/// sans plus de droits qu'un montage `bind`.
const BIND_VOLUME_OPTIONS: &[&str] = &["bind", "rbind", "ro", "rw", "nosuid", "nodev", "noexec"];

/// Champs d'un volume de premier niveau.
const VOLUME_FIELDS: &[&str] = &["name", "driver", "driver_opts", "external", "labels"];

/// Champs d'un réseau de premier niveau.
const NETWORK_FIELDS: &[&str] = &[
    "name",
    "driver",
    "driver_opts",
    "external",
    "attachable",
    "enable_ipv4",
    "enable_ipv6",
    "internal",
    "ipam",
    "labels",
];

/// Options d'un réseau qui ne le branchent sur rien de la machine.
const NETWORK_DRIVER_OPTS: &[&str] = &[
    "com.docker.network.driver.mtu",
    "com.docker.network.bridge.enable_icc",
    "com.docker.network.bridge.enable_ip_masquerade",
];

/// Champs d'un `config` ou d'un `secret` de premier niveau.
const FILE_SOURCE_FIELDS: &[&str] = &["name", "file", "content", "environment", "labels"];

/// Espaces de noms d'un service : le champ, son nom dans les permissions, les valeurs qui restent
/// dans le conteneur, et s'il peut rejoindre celui d'un autre service ou d'un autre conteneur.
const NAMESPACES: [(&str, &str, &[&str], bool); 5] = [
    ("pid", "pid", &[], true),
    ("ipc", "ipc", &["none", "private", "shareable"], true),
    ("uts", "uts", &[], false),
    ("userns_mode", "userns", &[], false),
    ("cgroup", "cgroup", &["private"], false),
];

/// Ce que l'analyse doit savoir en plus de la configuration.
#[derive(Clone, Copy, Debug)]
pub struct ComposeContext<'a> {
    /// Slug de l'app.
    pub app: &'a str,
    /// Hôte du serveur, avec son port s'il en a un : `updates.drakbase.com`.
    pub host: &'a str,
    /// Dossier du projet Compose, absolu, sans `/` final.
    pub project_dir: &'a str,
    pub service: &'a str,
    pub pre_update: Option<&'a str>,
    /// Images servies par DrakBase, de `stack.json`.
    pub images: &'a BTreeMap<String, Digest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionKind {
    NetworkHost,
    NetworkContainer,
    Namespace,
    NamespaceContainer,
    VolumesContainer,
    Runtime,
    Privileged,
    Capability,
    Device,
    DeviceRule,
    SecurityOpt,
    DockerSocket,
    HostPath,
    Port,
    ExternalVolume,
    SharedVolume,
    VolumeHostPath,
    ExternalNetwork,
    SharedNetwork,
}

impl PermissionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            PermissionKind::NetworkHost => "network_host",
            PermissionKind::NetworkContainer => "network_container",
            PermissionKind::Namespace => "namespace",
            PermissionKind::NamespaceContainer => "namespace_container",
            PermissionKind::VolumesContainer => "volumes_container",
            PermissionKind::Runtime => "runtime",
            PermissionKind::Privileged => "privileged",
            PermissionKind::Capability => "capability",
            PermissionKind::Device => "device",
            PermissionKind::DeviceRule => "device_rule",
            PermissionKind::SecurityOpt => "security_opt",
            PermissionKind::DockerSocket => "docker_socket",
            PermissionKind::HostPath => "host_path",
            PermissionKind::Port => "port",
            PermissionKind::ExternalVolume => "external_volume",
            PermissionKind::SharedVolume => "shared_volume",
            PermissionKind::VolumeHostPath => "volume_host_path",
            PermissionKind::ExternalNetwork => "external_network",
            PermissionKind::SharedNetwork => "shared_network",
        }
    }
}

/// Ce qu'une stack demande à la machine. La clé (`key`) est ce que l'agent retient et compare ;
/// `detail` ne sert qu'au libellé.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    /// Service concerné, vide pour ce qui concerne tout le projet.
    pub service: String,
    pub kind: PermissionKind,
    pub value: String,
    pub detail: Option<String>,
}

impl Permission {
    fn new(service: &str, kind: PermissionKind, value: impl Into<String>) -> Self {
        Self {
            service: service.to_owned(),
            kind,
            value: value.into(),
            detail: None,
        }
    }

    /// Clé stable : `<service>|<type>|<valeur>`.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.service, self.kind.as_str(), self.value)
    }

    /// Libellé en français, sans le service.
    pub fn label(&self) -> String {
        let value = &self.value;
        match self.kind {
            PermissionKind::NetworkHost => "réseau de l'hôte".to_owned(),
            PermissionKind::NetworkContainer => format!("réseau du conteneur {value}"),
            PermissionKind::Namespace => format!("espace {value} de l'hôte"),
            PermissionKind::NamespaceContainer => match value.split_once(':') {
                Some((namespace, container)) => {
                    format!("espace {namespace} du conteneur {container}")
                }
                None => format!("espace du conteneur {value}"),
            },
            PermissionKind::VolumesContainer => format!("volumes du conteneur {value}"),
            PermissionKind::Runtime => format!("moteur d'exécution {value}"),
            PermissionKind::Privileged => {
                "mode privilégié : tous les droits sur la machine".to_owned()
            }
            PermissionKind::Capability => format!("capacité {value}"),
            PermissionKind::Device => format!("périphérique {value}"),
            PermissionKind::DeviceRule => format!("règle de périphériques {value}"),
            PermissionKind::SecurityOpt => format!("protection modifiée : {value}"),
            PermissionKind::DockerSocket if is_socket_file(value) => {
                format!("socket Docker {value} : contrôle total de Docker, donc de la machine")
            }
            PermissionKind::DockerSocket => format!(
                "dossier {value}, qui contient la socket Docker : contrôle total de Docker, donc de la machine"
            ),
            PermissionKind::HostPath => match value.rsplit_once(':') {
                Some((path, "ro")) => format!("dossier de l'hôte {path}, en lecture seule"),
                Some((path, _)) if is_system_path(path) => {
                    format!("dossier de l'hôte {path}, en écriture : contrôle de la machine")
                }
                Some((path, _)) => format!("dossier de l'hôte {path}"),
                None => format!("dossier de l'hôte {value}"),
            },
            PermissionKind::Port => {
                // <adresse>:<port>/<protocole>, l'adresse pouvant être en IPv6.
                let (address, port) = value.rsplit_once(':').unwrap_or(("*", value));
                let port = match port.split_once('/') {
                    Some(("auto", protocol)) => format!("choisi par Docker ({protocol})"),
                    _ => port.to_owned(),
                };
                if address == "*" {
                    format!("port {port} sur toutes les adresses")
                } else {
                    format!("port {port} sur {address}")
                }
            }
            PermissionKind::ExternalVolume => format!("volume existant {value}"),
            PermissionKind::SharedVolume => {
                format!("volume à nom fixe {value}, qu'une autre stack peut partager")
            }
            // Le libellé est fait avec le volume, là où son type et ses options sont connus.
            PermissionKind::VolumeHostPath => self
                .detail
                .clone()
                .unwrap_or_else(|| format!("volume monté {value}")),
            PermissionKind::ExternalNetwork => format!("réseau existant {value}"),
            PermissionKind::SharedNetwork => {
                format!("réseau à nom fixe {value}, qu'une autre stack peut partager")
            }
        }
    }
}

impl fmt::Display for Permission {
    /// `<service> : <libellé>`, ou le libellé seul pour tout le projet.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.service.is_empty() {
            f.write_str(&self.label())
        } else {
            write!(f, "{} : {}", self.service, self.label())
        }
    }
}

/// Remarque qui ne bloque pas la publication.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Warning {
    pub service: String,
    /// `container_name`.
    pub kind: &'static str,
    pub value: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} : container_name « {} » est déconseillé, il peut entrer en conflit avec un conteneur existant",
            self.service, self.value
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposeReport {
    /// Triées par clé, sans doublon.
    pub permissions: Vec<Permission>,
    pub warnings: Vec<Warning>,
}

fn invalid(what: impl Into<String>) -> StackError {
    StackError::ComposeJsonInvalid(what.into())
}

fn object<'v>(value: &'v Value, what: &str) -> Result<&'v Map<String, Value>, StackError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{what} : objet attendu")))
}

/// Liste facultative : absente ou `null` donne une liste vide.
fn list<'v>(service: &'v Map<String, Value>, field: &str) -> Result<&'v [Value], StackError> {
    match service.get(field) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(invalid(format!("{field} : liste attendue"))),
    }
}

fn strings<'v>(service: &'v Map<String, Value>, field: &str) -> Result<Vec<&'v str>, StackError> {
    list(service, field)?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| invalid(format!("{field} : chaînes attendues")))
        })
        .collect()
}

fn text<'v>(map: &'v Map<String, Value>, field: &str) -> Option<&'v str> {
    map.get(field).and_then(Value::as_str)
}

/// Un texte facultatif : absent ou `null` donne `None`, autre chose qu'une chaîne est refusé.
fn optional_text<'v>(
    map: &'v Map<String, Value>,
    field: &str,
) -> Result<Option<&'v str>, StackError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(format!("{field} : chaîne attendue"))),
    }
}

/// Un booléen facultatif : absent ou `null` vaut faux, autre chose qu'un booléen est refusé.
fn flag(map: &Map<String, Value>, field: &str) -> Result<bool, StackError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid(format!("{field} : booléen attendu"))),
    }
}

/// Les chemins d'un `env_file` ou d'un `label_file` : une chaîne, ou une liste de chaînes et
/// d'objets `{"path": …}`.
fn file_paths<'v>(value: Option<&'v Value>, field: &str) -> Result<Vec<&'v str>, StackError> {
    let unreadable = || invalid(format!("{field} : chemin attendu"));
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(path)) => return Ok(vec![path]),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(unreadable()),
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(path) => Ok(path.as_str()),
            Value::Object(entry) => text(entry, "path").ok_or_else(unreadable),
            _ => Err(unreadable()),
        })
        .collect()
}

/// Refuse le premier champ de `map`, dans l'ordre des noms, qui n'est dans aucune des listes
/// `allowed`. Un champ `null` est absent, et une extension `x-…` est ignorée par Compose.
fn check_fields(
    map: &Map<String, Value>,
    allowed: &[&[&str]],
    path: &str,
) -> Result<(), StackError> {
    for key in sorted_keys(map) {
        let known = allowed.iter().any(|list| list.contains(&key.as_str()));
        if known || key.starts_with("x-") || map[key].is_null() {
            continue;
        }
        return Err(StackError::ComposeFieldUnsupported(join_path(path, key)));
    }
    Ok(())
}

/// `services.app` et `provider` donnent `services.app.provider`.
fn join_path(path: &str, field: &str) -> String {
    if path.is_empty() {
        field.to_owned()
    } else {
        format!("{path}.{field}")
    }
}

/// Le nom est-il celui que Compose donne seul à un volume ou à un réseau de la stack,
/// `<app>_<clé>` ? Une clé qui contient `_` n'en profite pas : `demo_x_data` est aussi le volume
/// `data` d'un projet `demo_x`, que Compose rejoindrait sans rien dire. L'identifiant d'une app
/// n'a pas de `_` : sans `_` dans la clé, le nom ne peut être que celui de la stack.
fn own_name(app: &str, key: &str, name: &str) -> bool {
    !key.contains('_') && name == format!("{app}_{key}")
}

fn unsupported_value(field: String, value: &str) -> StackError {
    StackError::ComposeValueUnsupported {
        field,
        value: value.to_owned(),
    }
}

/// Un chemin monté est fait de composants ordinaires. Avec `.` ou `..`,
/// `<projet>/files/../../etc` commencerait comme un fichier de la stack, et en sortirait sans
/// qu'aucune permission ne soit montrée.
fn check_mounted_path(path: &str) -> Result<(), StackError> {
    if path
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        return Err(StackError::BindPathInvalid(path.to_owned()));
    }
    Ok(())
}

/// Le chemin, ses barres répétées réduites à une seule. Docker Compose les garde telles quelles
/// (`//var/run`, ou `/srv/data//photos` quand un réglage de `.env` finit par une barre), et les
/// règles sur les chemins comparent du texte : elles jugent le chemin réduit, que porte aussi la
/// valeur d'une permission.
fn collapse_slashes(path: &str) -> String {
    let mut collapsed = String::with_capacity(path.len());
    for c in path.chars() {
        if c != '/' || !collapsed.ends_with('/') {
            collapsed.push(c);
        }
    }
    collapsed
}

/// Dossiers de l'agent sur la machine : l'état de chaque stack (permissions acceptées, clés de
/// confiance, `.env`) et sa configuration. Un conteneur qui les écrirait s'accorderait ce qu'il
/// veut ; aucune stack ne les monte.
pub const AGENT_DIRS: [&str; 2] = [AGENT_STATE_DIR, AGENT_CONFIG_DIR];
/// Dossier des stacks de l'agent : état, versions et `.env` de chacune.
pub const AGENT_STATE_DIR: &str = "/var/lib/drakbase-stack";
/// Dossier de la configuration de l'agent.
pub const AGENT_CONFIG_DIR: &str = "/etc/drakbase-stack";

/// Dossiers du système : en écriture, chacun donne la machine (tâches planifiées, comptes,
/// programmes, périphériques, données de Docker).
const SYSTEM_DIRS: &[&str] = &[
    "/etc",
    "/root",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/var/lib/docker",
    "/var/lib/containerd",
    // Les tâches planifiées de root.
    "/var/spool",
];

/// `path` est-il `dir`, ou dans `dir` ?
fn is_within(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Le chemin est-il un dossier de l'agent, ou dans l'un d'eux ?
fn in_agent_dirs(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    AGENT_DIRS.iter().any(|dir| is_within(path, dir))
}

/// Le chemin est-il un dossier du système, dans l'un d'eux, ou un dossier qui en contient un
/// (`/var/lib`) ?
fn is_system_path(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    SYSTEM_DIRS
        .iter()
        .any(|dir| is_within(path, dir) || is_within(dir, path))
}

/// Le chemin est-il un fichier de socket, d'après son nom ?
fn is_socket_file(path: &str) -> bool {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|name| name.ends_with(".sock"))
}

/// Dossiers de `/run` et de `/var/run` où un moteur de conteneurs pose sa socket ; `user` est
/// celui des sessions, où la pose un Docker sans root.
const ENGINE_RUN_DIRS: &[&str] = &["docker", "containerd", "podman", "crio", "user"];

/// Le chemin mène-t-il à la socket d'un moteur de conteneurs ? Un fichier nommé `docker.sock`,
/// un fichier `.sock` de `/run` ou de `/var/run`, ou un dossier qui la contient : `/`, `/var`,
/// `/run`, `/var/run`, et sous ces deux derniers les dossiers des moteurs. Monter le dossier
/// donne la socket autant que la monter elle-même.
fn is_docker_socket(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/var" || path.rsplit('/').next() == Some("docker.sock") {
        return true;
    }
    let Some(rest) = path
        .strip_prefix("/var/run")
        .or_else(|| path.strip_prefix("/run"))
    else {
        return false;
    };
    match rest.strip_prefix('/') {
        // `/run` ou `/var/run` lui-même ; `/runner` n'en est pas.
        None => rest.is_empty(),
        Some(inside) => {
            is_socket_file(path)
                || inside
                    .split('/')
                    .next()
                    .is_some_and(|first| ENGINE_RUN_DIRS.contains(&first))
        }
    }
}

/// Un objet facultatif : absent ou `null` donne `None`, autre chose qu'un objet est refusé.
fn optional_object<'v>(
    map: &'v Map<String, Value>,
    field: &str,
) -> Result<Option<&'v Map<String, Value>>, StackError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => object(value, field).map(Some),
    }
}

/// L'objet `bind` d'un montage de service, contrôlé avec sa propre liste de champs. Refuse une
/// propagation qui ferait remonter vers la machine ce que le conteneur monte. Rend vrai si un
/// montage `read_only` le reste en entier : faux avec `recursive: writable`, qui laisse ses
/// sous-montages en écriture.
fn check_bind(mount: &Map<String, Value>, path: &str) -> Result<bool, StackError> {
    let Some(bind) = optional_object(mount, "bind")? else {
        return Ok(true);
    };
    let path = join_path(path, "bind");
    check_fields(bind, &[MOUNT_BIND_FIELDS], &path)?;
    if let Some(propagation) = optional_text(bind, "propagation")?
        .filter(|value| !value.is_empty() && !BIND_PROPAGATIONS.contains(value))
    {
        return Err(unsupported_value(
            join_path(&path, "propagation"),
            propagation,
        ));
    }
    // `z` et `Z` font changer par Docker l'étiquette SELinux du dossier, sur la machine.
    if let Some(selinux) = optional_text(bind, "selinux")?.filter(|value| !value.is_empty()) {
        return Err(unsupported_value(join_path(&path, "selinux"), selinux));
    }
    match optional_text(bind, "recursive")? {
        None | Some("") => Ok(true),
        Some(recursive) if READ_ONLY_RECURSIVE.contains(&recursive) => Ok(true),
        Some("writable") => Ok(false),
        Some(other) => Err(unsupported_value(join_path(&path, "recursive"), other)),
    }
}

/// Refuse un texte qui porte un caractère de contrôle, de sens d'écriture ou invisible : affiché
/// dans la liste des permissions à accepter, il pourrait en effacer ou en cacher une ligne.
fn check_visible(field: &str, value: &str) -> Result<(), StackError> {
    if crate::visible::has_hidden(field) || crate::visible::has_hidden(value) {
        return Err(StackError::ComposeTextHidden {
            field: crate::visible::visible(field),
            value: crate::visible::visible(value),
        });
    }
    Ok(())
}

/// Les noms que montrent les permissions et les messages : ceux des services, et ceux des
/// volumes, réseaux, `configs` et `secrets` de premier niveau, clé et `name`.
fn check_names(root: &Map<String, Value>) -> Result<(), StackError> {
    for section in ["services", "volumes", "networks", "configs", "secrets"] {
        let Some(entries) = root.get(section).and_then(Value::as_object) else {
            continue;
        };
        for key in sorted_keys(entries) {
            check_visible(section, key)?;
            if let Some(name) = entries[key].get("name").and_then(Value::as_str) {
                check_visible(&join_path(&join_path(section, key), "name"), name)?;
            }
        }
    }
    Ok(())
}

/// L'`ipam` d'un réseau : Docker choisit ses adresses. Des adresses données par la stack
/// (`config`) pourraient recouvrir le réseau local de la machine.
fn check_ipam(ipam: &Map<String, Value>, path: &str) -> Result<(), StackError> {
    for key in sorted_keys(ipam) {
        let value = &ipam[key];
        let empty = match value {
            Value::Null => true,
            Value::Array(items) => items.is_empty(),
            Value::Object(entries) => entries.is_empty(),
            _ => false,
        };
        match key.as_str() {
            "driver" => {
                if let Some(driver) = optional_text(ipam, "driver")?
                    .filter(|driver| !matches!(*driver, "" | "default"))
                {
                    return Err(unsupported_value(join_path(path, "driver"), driver));
                }
            }
            _ if empty => {}
            _ => return Err(StackError::ComposeFieldUnsupported(join_path(path, key))),
        }
    }
    Ok(())
}

/// Ressources d'un service : rien qui le fasse passer avant le reste de la machine. Le noyau
/// tuerait d'autres services avant un conteneur à l'abri du manque de mémoire, un temps réel
/// réservé affamerait leurs processus, et `blkio_config` peut brider un disque de la machine.
fn check_resources(service: &Map<String, Value>, path: &str) -> Result<(), StackError> {
    if flag(service, "oom_kill_disable")? {
        return Err(unsupported_value(
            join_path(path, "oom_kill_disable"),
            "true",
        ));
    }
    // Un nombre, ou sa forme en texte ; pour `cpu_rt_runtime`, une durée (« 400ms »).
    for (field, refused) in [
        ("oom_score_adj", (|number| number < 0.0) as fn(f64) -> bool),
        ("cpu_rt_runtime", |number| number > 0.0),
    ] {
        let shown = match service.get(field) {
            None | Some(Value::Null) => continue,
            Some(Value::Number(number)) => {
                if !number.as_f64().is_some_and(refused) {
                    continue;
                }
                number.to_string()
            }
            Some(Value::String(text)) => {
                let text = text.trim();
                // Une durée s'écrit avec son unité : « 0s » ne réserve rien non plus.
                let number = text.trim_end_matches(|c: char| c.is_alphabetic());
                match number.trim_end().parse::<f64>() {
                    Ok(number) if !refused(number) => continue,
                    _ if text.is_empty() => continue,
                    _ => text.to_owned(),
                }
            }
            Some(other) => other.to_string(),
        };
        return Err(unsupported_value(join_path(path, field), &shown));
    }
    if let Some(blkio) = optional_object(service, "blkio_config")? {
        let path = join_path(path, "blkio_config");
        check_fields(blkio, &[BLKIO_FIELDS], &path)?;
        for field in BLKIO_FIELDS.iter().filter(|field| **field != "weight") {
            let empty = match blkio.get(*field) {
                None | Some(Value::Null) => true,
                Some(Value::Array(items)) => items.is_empty(),
                Some(_) => false,
            };
            if !empty {
                return Err(StackError::ComposeFieldUnsupported(join_path(&path, field)));
            }
        }
    }
    Ok(())
}

/// Blancs admis dans une ligne de `.env` : l'espace et la tabulation.
const ENV_BLANKS: [char; 2] = [' ', '\t'];

/// Nom de la variable d'une ligne `NOM=valeur` d'un fichier `.env`, précédée ou non de `export` :
/// la seule forme que l'agent lit. `None` pour toute autre ligne.
pub fn env_variable(line: &str) -> Option<&str> {
    env_assignment(line).map(|(name, _)| name)
}

/// Comme [`env_variable`], en admettant aussi `NOM: valeur`, que Docker Compose lit : pour le
/// `.env` de la machine, que l'utilisateur écrit à sa guise.
pub fn env_variable_as_written(line: &str) -> Option<&str> {
    env_entry(line, &['=', ':']).map(|(name, _)| name)
}

/// Nom et valeur (sans ses blancs de tête) d'une ligne `NOM=valeur`.
fn env_assignment(line: &str) -> Option<(&str, &str)> {
    env_entry(line, &['='])
}

/// Nom et valeur (sans ses blancs de tête) d'une ligne `NOM<séparateur>valeur`.
fn env_entry<'a>(line: &'a str, separators: &[char]) -> Option<(&'a str, &'a str)> {
    let line = line.trim_start_matches(ENV_BLANKS);
    let line = match line.strip_prefix("export") {
        Some(rest) if rest.starts_with(ENV_BLANKS) => rest.trim_start_matches(ENV_BLANKS),
        _ => line,
    };
    let end = line
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(line.len());
    let (name, rest) = line.split_at(end);
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let value = rest
        .trim_start_matches(ENV_BLANKS)
        .strip_prefix(separators)?;
    starts_well.then_some((name, value.trim_start_matches(ENV_BLANKS)))
}

/// Une valeur qui s'ouvre par un guillemet ou une apostrophe finit-elle sa ligne ? Comme pour
/// Docker Compose, le premier caractère pareil qui ne suit pas une barre oblique inverse la
/// referme : sans lui, Compose lirait les lignes suivantes comme sa suite. Après lui, il ne vient
/// que des blancs ou un commentaire : Compose lirait autre chose comme une seconde variable.
fn env_value_ends_its_line(value: &str) -> bool {
    let Some(quote @ ('"' | '\'')) = value.chars().next() else {
        return true;
    };
    let mut previous = quote;
    for (index, c) in value.char_indices().skip(1) {
        if c == quote && previous != '\\' {
            let rest = value[index + c.len_utf8()..].trim_start_matches(ENV_BLANKS);
            return rest.is_empty() || rest.starts_with('#');
        }
        previous = c;
    }
    false
}

/// Contrôle le `.env.example` d'une archive.
/// - Chaque ligne est vide, un commentaire ou une seule `NOM=valeur`, la valeur tenant sur la
///   ligne. Docker Compose lit d'autres formes (`NOM: valeur`, un nom seul, une valeur entre
///   guillemets sur plusieurs lignes, une seconde variable après un guillemet fermant) que
///   l'agent, qui complète le `.env` ligne par ligne, ne verrait pas comme lui.
/// - Aucun nom de variable ne commence par `COMPOSE_` ni `DOCKER_`. Docker Compose lit ses
///   propres réglages (`COMPOSE_PROFILES`…) dans le fichier que lui donne l'agent, où ces
///   variables seraient recopiées.
pub fn check_env_example(env_example: &[u8]) -> Result<(), StackError> {
    let text = String::from_utf8_lossy(env_example);
    for (index, line) in text.split('\n').enumerate() {
        let unsupported = || StackError::EnvLineUnsupported { line: index + 1 };
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line
            .chars()
            .any(|c| (c.is_control() && c != '\t') || c == char::REPLACEMENT_CHARACTER)
        {
            return Err(unsupported());
        }
        let content = line.trim_start_matches(ENV_BLANKS);
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let (name, value) = env_assignment(line).ok_or_else(unsupported)?;
        if ["COMPOSE_", "DOCKER_"]
            .iter()
            .any(|reserved| name.starts_with(reserved))
        {
            return Err(StackError::EnvNameReserved(name.to_owned()));
        }
        if !env_value_ends_its_line(value) {
            return Err(unsupported());
        }
    }
    Ok(())
}

/// Clés d'un objet JSON triées, quel que soit l'ordre que garde serde_json.
fn sorted_keys(map: &Map<String, Value>) -> Vec<&String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys
}

struct Analysis<'a> {
    context: &'a ComposeContext<'a>,
    permissions: BTreeMap<String, Permission>,
    warnings: BTreeSet<Warning>,
    used_images: BTreeSet<String>,
}

impl Analysis<'_> {
    fn grant(&mut self, permission: Permission) {
        self.permissions
            .entry(permission.key())
            .or_insert(permission);
    }

    fn in_project(&self, path: &str) -> bool {
        let dir = self.context.project_dir;
        path == dir
            || path
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    fn in_files(&self, path: &str) -> bool {
        path.strip_prefix(self.context.project_dir)
            .and_then(|rest| rest.strip_prefix("/files/"))
            .is_some_and(|rest| !rest.is_empty())
    }

    fn check_image(&mut self, name: &str, service: &Map<String, Value>) -> Result<(), StackError> {
        if service.get("build").is_some_and(|build| !build.is_null()) {
            return Err(StackError::BuildPresent(name.to_owned()));
        }
        let image =
            text(service, "image").ok_or_else(|| StackError::ImageMissing(name.to_owned()))?;
        let not_pinned = || StackError::ImageNotPinned {
            service: name.to_owned(),
            image: image.to_owned(),
        };
        let (reference, digest) = image.split_once('@').ok_or_else(not_pinned)?;
        let digest = Digest::parse(digest).map_err(|_| not_pinned())?;

        let Some(rest) = reference
            .strip_prefix(self.context.host)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return Ok(()); // image d'un autre registre, épinglée
        };
        let malformed = || StackError::ImageMalformed {
            service: name.to_owned(),
            image: image.to_owned(),
        };
        let (app, image_name) = rest.split_once('/').ok_or_else(malformed)?;
        if !is_app_slug(app) || !is_image_name(image_name) {
            return Err(malformed());
        }
        if app != self.context.app {
            return Err(StackError::ImageOtherApp {
                service: name.to_owned(),
                image: image.to_owned(),
            });
        }
        if self.context.images.get(image_name) != Some(&digest) {
            return Err(StackError::ImageNotDeclared {
                service: name.to_owned(),
                image: image.to_owned(),
            });
        }
        self.used_images.insert(image_name.to_owned());
        Ok(())
    }

    fn check_profiles(&self, name: &str, service: &Map<String, Value>) -> Result<(), StackError> {
        let profiles = strings(service, "profiles")?;
        if Some(name) == self.context.pre_update {
            if profiles != [HOOKS_PROFILE] {
                return Err(StackError::PreUpdateProfile(name.to_owned()));
            }
        } else if !profiles.is_empty() {
            return Err(StackError::ProfileUnexpected(name.to_owned()));
        }
        Ok(())
    }

    fn check_volumes(
        &mut self,
        name: &str,
        service: &Map<String, Value>,
    ) -> Result<(), StackError> {
        let path = format!("services.{name}.volumes");
        for volume in list(service, "volumes")? {
            let volume = object(volume, "volumes")?;
            check_fields(volume, &[MOUNT_FIELDS], &path)?;
            match text(volume, "type") {
                Some("bind") => {}
                Some("volume") => {
                    if let Some(options) = optional_object(volume, "volume")? {
                        check_fields(options, &[MOUNT_VOLUME_FIELDS], &join_path(&path, "volume"))?;
                        if let Some(subpath) = optional_text(options, "subpath")? {
                            check_mounted_path(subpath)?;
                        }
                    }
                    continue;
                }
                Some("tmpfs") => {
                    if let Some(options) = optional_object(volume, "tmpfs")? {
                        check_fields(options, &[MOUNT_TMPFS_FIELDS], &join_path(&path, "tmpfs"))?;
                    }
                    continue;
                }
                Some(other) => return Err(unsupported_value(join_path(&path, "type"), other)),
                None => return Err(invalid("volumes : type absent")),
            }
            let source = collapse_slashes(
                text(volume, "source").ok_or_else(|| invalid("volumes : source absente"))?,
            );
            let source = source.as_str();
            check_mounted_path(source)?;
            // Contrôlé même sans `read_only` : la propagation l'est toujours.
            let keeps_read_only = check_bind(volume, &path)?;
            let read_only = flag(volume, "read_only")? && keeps_read_only;
            if self.in_project(source) {
                if !self.in_files(source) {
                    return Err(StackError::BindOutsideFiles(source.to_owned()));
                }
                if !read_only {
                    return Err(StackError::BindNotReadOnly(source.to_owned()));
                }
            } else if in_agent_dirs(source) {
                return Err(unsupported_value(join_path(&path, "source"), source));
            } else if is_docker_socket(source) {
                self.grant(Permission::new(name, PermissionKind::DockerSocket, source));
            } else {
                let mode = if read_only { "ro" } else { "rw" };
                self.grant(Permission::new(
                    name,
                    PermissionKind::HostPath,
                    format!("{source}:{mode}"),
                ));
            }
        }
        Ok(())
    }

    /// Périphériques demandés à Docker (`gpus`, réservations de `deploy`) : chacun de leurs
    /// `device_ids`, sinon `gpu`.
    fn reserve_devices(&mut self, name: &str, requests: &[Value]) -> Result<(), StackError> {
        for request in requests {
            let ids = strings(object(request, "périphériques")?, "device_ids")?;
            if ids.is_empty() {
                self.grant(Permission::new(name, PermissionKind::Device, "gpu"));
            }
            for id in ids {
                self.grant(Permission::new(name, PermissionKind::Device, id));
            }
        }
        Ok(())
    }

    fn collect_service(
        &mut self,
        name: &str,
        service: &Map<String, Value>,
    ) -> Result<(), StackError> {
        let path = format!("services.{name}");
        match optional_text(service, "network_mode")? {
            None | Some("" | "bridge" | "none") => {}
            Some("host") => self.grant(Permission::new(name, PermissionKind::NetworkHost, "host")),
            Some(mode) => {
                if let Some(container) = mode.strip_prefix("container:") {
                    self.grant(Permission::new(
                        name,
                        PermissionKind::NetworkContainer,
                        container,
                    ));
                } else if !mode.starts_with("service:") {
                    return Err(unsupported_value(join_path(&path, "network_mode"), mode));
                }
            }
        }
        for (field, namespace, plain, joins) in NAMESPACES {
            let Some(mode) = optional_text(service, field)? else {
                continue;
            };
            if mode == "host" {
                self.grant(Permission::new(name, PermissionKind::Namespace, namespace));
            } else if mode.is_empty()
                || plain.contains(&mode)
                || (joins && mode.starts_with("service:"))
            {
                // Reste dans le conteneur, ou rejoint un service de la même stack.
            } else if let Some(container) = mode.strip_prefix("container:").filter(|_| joins) {
                self.grant(Permission::new(
                    name,
                    PermissionKind::NamespaceContainer,
                    format!("{namespace}:{container}"),
                ));
            } else {
                return Err(unsupported_value(join_path(&path, field), mode));
            }
        }
        if flag(service, "privileged")? {
            self.grant(Permission::new(name, PermissionKind::Privileged, "true"));
        }
        check_resources(service, &path)?;
        // Une commande lancée dans le conteneur avec tous les droits vaut le mode privilégié.
        for field in ["post_start", "pre_stop"] {
            for hook in list(service, field)? {
                if flag(object(hook, field)?, "privileged")? {
                    self.grant(Permission::new(name, PermissionKind::Privileged, "true"));
                }
            }
        }
        for source in strings(service, "volumes_from")? {
            // `container:<nom>[:ro|:rw]` ; sans ce préfixe, c'est un service de la stack.
            if let Some(container) = source.strip_prefix("container:") {
                let container = container
                    .strip_suffix(":ro")
                    .or_else(|| container.strip_suffix(":rw"))
                    .unwrap_or(container);
                self.grant(Permission::new(
                    name,
                    PermissionKind::VolumesContainer,
                    container,
                ));
            }
        }
        if let Some(runtime) = optional_text(service, "runtime")?.filter(|name| !name.is_empty()) {
            self.grant(Permission::new(name, PermissionKind::Runtime, runtime));
        }
        match service.get("gpus") {
            None | Some(Value::Null) => {}
            Some(Value::Array(requests)) => self.reserve_devices(name, requests)?,
            // `gpus: all`, que Compose 2.29 peut laisser en texte.
            Some(_) => self.grant(Permission::new(name, PermissionKind::Device, "gpu")),
        }
        let reserved = service
            .get("deploy")
            .and_then(|deploy| deploy.get("resources"))
            .and_then(|resources| resources.get("reservations"))
            .and_then(|reservations| reservations.get("devices"));
        match reserved {
            None | Some(Value::Null) => {}
            Some(Value::Array(requests)) => self.reserve_devices(name, requests)?,
            Some(_) => {
                return Err(invalid(
                    "deploy.resources.reservations.devices : liste attendue",
                ));
            }
        }
        for field in ["env_file", "label_file"] {
            for file in file_paths(service.get(field), field)? {
                let file = collapse_slashes(file);
                check_mounted_path(&file)?;
                if !self.in_files(&file) {
                    return Err(StackError::EnvFileOutsideFiles(file));
                }
            }
        }
        if let Some(logging) = optional_object(service, "logging")? {
            let logging_path = join_path(&path, "logging");
            check_fields(logging, &[LOGGING_FIELDS], &logging_path)?;
            match optional_text(logging, "driver")?.filter(|driver| !driver.is_empty()) {
                // Avec un autre pilote, c'est le service Docker qui se connecte, depuis la
                // machine, à l'adresse que donne la stack.
                Some(driver) if !LOGGING_DRIVERS.contains(&driver) => {
                    return Err(unsupported_value(
                        join_path(&logging_path, "driver"),
                        driver,
                    ));
                }
                Some(_) => {}
                // Le pilote par défaut de la machine peut être l'un de ceux-là : il ne reçoit
                // que des options de taille.
                None => {
                    if let Some(options) = optional_object(logging, "options")? {
                        check_fields(
                            options,
                            &[DEFAULT_LOGGING_OPTIONS],
                            &join_path(&logging_path, "options"),
                        )?;
                    }
                }
            }
        }
        for capability in strings(service, "cap_add")? {
            self.grant(Permission::new(
                name,
                PermissionKind::Capability,
                capability.to_ascii_uppercase(),
            ));
        }
        for device in list(service, "devices")? {
            // Compose 2.29 écrit « source:cible[:droits] », Compose 5.5 un objet.
            let source = match device {
                Value::String(spec) => spec.split(':').next().unwrap_or(spec),
                Value::Object(map) => {
                    text(map, "source").ok_or_else(|| invalid("devices : source absente"))?
                }
                _ => return Err(invalid("devices : chaîne ou objet attendu")),
            };
            self.grant(Permission::new(name, PermissionKind::Device, source));
        }
        for rule in strings(service, "device_cgroup_rules")? {
            self.grant(Permission::new(name, PermissionKind::DeviceRule, rule));
        }
        for option in strings(service, "security_opt")? {
            let compact = option.replace('=', ":");
            if !matches!(
                compact.as_str(),
                "no-new-privileges" | "no-new-privileges:true"
            ) {
                self.grant(Permission::new(name, PermissionKind::SecurityOpt, option));
            }
        }
        for port in list(service, "ports")? {
            let port = object(port, "ports")?;
            let published = match port.get("published") {
                Some(Value::String(value)) if !value.is_empty() => value.clone(),
                Some(Value::Number(value)) => value.to_string(),
                _ => "auto".to_owned(),
            };
            let address = text(port, "host_ip")
                .filter(|ip| !matches!(*ip, "" | "0.0.0.0" | "::"))
                .unwrap_or("*");
            let protocol = text(port, "protocol").unwrap_or("tcp");
            self.grant(Permission::new(
                name,
                PermissionKind::Port,
                format!("{address}:{published}/{protocol}"),
            ));
        }
        if let Some(container_name) = text(service, "container_name") {
            self.warnings.insert(Warning {
                service: name.to_owned(),
                kind: "container_name",
                value: container_name.to_owned(),
            });
        }
        Ok(())
    }

    /// `configs` et `secrets` de haut niveau qui lisent un fichier.
    fn check_file_sources(
        &mut self,
        root: &Map<String, Value>,
        field: &str,
    ) -> Result<(), StackError> {
        let Some(entries) = root.get(field).filter(|value| !value.is_null()) else {
            return Ok(());
        };
        let entries = object(entries, field)?;
        for key in sorted_keys(entries) {
            let entry = object(&entries[key], field)?;
            check_fields(entry, &[FILE_SOURCE_FIELDS], &join_path(field, key))?;
            let Some(file) = text(entry, "file") else {
                continue;
            };
            let file = collapse_slashes(file);
            let file = file.as_str();
            check_mounted_path(file)?;
            if self.in_project(file) {
                if !self.in_files(file) {
                    return Err(StackError::BindOutsideFiles(file.to_owned()));
                }
            } else if in_agent_dirs(file) {
                return Err(unsupported_value(
                    join_path(&join_path(field, key), "file"),
                    file,
                ));
            } else if is_docker_socket(file) {
                self.grant(Permission::new("", PermissionKind::DockerSocket, file));
            } else {
                self.grant(Permission::new(
                    "",
                    PermissionKind::HostPath,
                    format!("{file}:ro"),
                ));
            }
        }
        Ok(())
    }

    /// Ce que monte un volume du pilote `local` qui a des `driver_opts` : un dossier de la machine
    /// (`none`, avec `bind` ou `rbind`). Le type et les options entrent dans la clé de la
    /// permission, avec le `device` : ce sont eux qui décident de ce qui est monté.
    fn grant_mounted_volume(
        &mut self,
        path: &str,
        volume_name: &str,
        options: &Map<String, Value>,
    ) -> Result<(), StackError> {
        let path = join_path(path, "driver_opts");
        check_fields(options, &[VOLUME_DRIVER_OPTS], &path)?;
        let kind = optional_text(options, "type")?.unwrap_or_default();
        let mount_options = optional_text(options, "o")?.unwrap_or_default();
        let device = collapse_slashes(optional_text(options, "device")?.unwrap_or_default());
        let device = device.as_str();
        // `nfs` et `cifs` passent leurs options à mount(2) : avec `o: bind`, un « partage » de
        // `device: /etc` monte le dossier de la machine. `proc` monterait le procfs de la
        // machine, `ext4` un de ses disques.
        if kind != "none" {
            return Err(unsupported_value(join_path(&path, "type"), kind));
        }
        // Les options sont prises dans une liste sans `|`, qui sépare les parties de la clé.
        let flags: Vec<&str> = mount_options.split(',').map(str::trim).collect();
        let binds = flags.iter().any(|flag| matches!(*flag, "bind" | "rbind"));
        if !binds || !flags.iter().all(|flag| BIND_VOLUME_OPTIONS.contains(flag)) {
            return Err(unsupported_value(join_path(&path, "o"), mount_options));
        }
        if !device.starts_with('/') {
            return Err(unsupported_value(join_path(&path, "device"), device));
        }
        check_mounted_path(device)?;
        if in_agent_dirs(device) {
            return Err(unsupported_value(join_path(&path, "device"), device));
        }
        if is_docker_socket(device) {
            self.grant(Permission::new("", PermissionKind::DockerSocket, device));
            return Ok(());
        }
        // `rbind` laisse ses sous-montages en écriture, et `rw` l'emporte sur `ro`.
        let has = |flag: &str| flags.contains(&flag);
        let read_only = if has("ro") && !has("rw") && !has("rbind") {
            ", en lecture seule"
        } else {
            ""
        };
        let label = format!("dossier de l'hôte {device}{read_only} (volume {volume_name})");
        let mut permission = Permission::new(
            "",
            PermissionKind::VolumeHostPath,
            format!("{kind}|{mount_options}|{device}"),
        );
        permission.detail = Some(label);
        self.grant(permission);
        Ok(())
    }

    fn collect_top_level(&mut self, root: &Map<String, Value>) -> Result<(), StackError> {
        if let Some(volumes) = root.get("volumes").filter(|value| !value.is_null()) {
            let volumes = object(volumes, "volumes")?;
            for key in sorted_keys(volumes) {
                let volume = match &volumes[key] {
                    Value::Null => continue,
                    other => object(other, "volumes")?,
                };
                let path = join_path("volumes", key);
                check_fields(volume, &[VOLUME_FIELDS], &path)?;
                if let Some(driver) = optional_text(volume, "driver")?
                    .filter(|driver| !matches!(*driver, "" | "local"))
                {
                    return Err(unsupported_value(join_path(&path, "driver"), driver));
                }
                let volume_name = text(volume, "name").unwrap_or(key);
                if flag(volume, "external")? {
                    self.grant(Permission::new(
                        "",
                        PermissionKind::ExternalVolume,
                        volume_name,
                    ));
                } else {
                    // Sans `name`, Compose le nomme `<app>_<clé>`.
                    let default_name = format!("{}_{key}", self.context.app);
                    let fixed = text(volume, "name").unwrap_or(&default_name);
                    if !own_name(self.context.app, key, fixed) {
                        // Compose monterait sans rien dire le volume du même nom d'une autre
                        // stack.
                        self.grant(Permission::new("", PermissionKind::SharedVolume, fixed));
                    }
                }
                if let Some(options) = volume.get("driver_opts").filter(|value| !value.is_null()) {
                    let options = object(options, "driver_opts")?;
                    if !options.is_empty() {
                        self.grant_mounted_volume(&path, volume_name, options)?;
                    }
                }
            }
        }
        if let Some(networks) = root.get("networks").filter(|value| !value.is_null()) {
            let networks = object(networks, "networks")?;
            for key in sorted_keys(networks) {
                let network = match &networks[key] {
                    Value::Null => continue,
                    other => object(other, "networks")?,
                };
                let path = join_path("networks", key);
                check_fields(network, &[NETWORK_FIELDS], &path)?;
                if let Some(driver) = optional_text(network, "driver")?
                    .filter(|driver| !matches!(*driver, "" | "bridge"))
                {
                    return Err(unsupported_value(join_path(&path, "driver"), driver));
                }
                if let Some(options) = network.get("driver_opts").filter(|value| !value.is_null()) {
                    check_fields(
                        object(options, "driver_opts")?,
                        &[NETWORK_DRIVER_OPTS],
                        &join_path(&path, "driver_opts"),
                    )?;
                }
                if let Some(ipam) = network.get("ipam").filter(|value| !value.is_null()) {
                    check_ipam(object(ipam, "ipam")?, &join_path(&path, "ipam"))?;
                }
                if flag(network, "external")? {
                    let network_name = text(network, "name").unwrap_or(key);
                    self.grant(Permission::new(
                        "",
                        PermissionKind::ExternalNetwork,
                        network_name,
                    ));
                } else {
                    let default_name = format!("{}_{key}", self.context.app);
                    let fixed = text(network, "name").unwrap_or(&default_name);
                    if !own_name(self.context.app, key, fixed) {
                        // Les conteneurs d'une autre stack peuvent se trouver sur un réseau de ce
                        // nom.
                        self.grant(Permission::new("", PermissionKind::SharedNetwork, fixed));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Ce qu'un chemin de la machine donne à une stack, au-delà d'un dossier ordinaire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPathKind {
    /// Un dossier de l'agent, ou ce qu'il contient.
    AgentDir,
    /// La socket d'un moteur de conteneurs, ou un dossier qui la contient.
    EngineSocket,
    /// Un dossier du système, ce qu'il contient, ou un dossier qui en contient un.
    SystemDir,
    Plain,
}

/// Un chemin de la machine que monte une stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSource {
    pub path: String,
    /// Monté en écriture par un service au moins. Le `file` d'un `config` ou d'un `secret` ne
    /// l'est jamais ; le `device` d'un volume est compté comme tel.
    pub writable: bool,
}

/// Un volume que Docker Compose crée pour la stack : un volume de haut niveau sans `external`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedVolume {
    pub name: String,
    /// Son nom n'est pas celui que Compose donne seul (`<app>_<clé>`) : une autre stack peut le
    /// partager (permission `shared_volume`).
    pub shared: bool,
}

/// Les volumes que la forme résolue d'un compose fait créer à Docker Compose pour `app`, et que
/// `docker compose down --volumes` supprimerait. Triés par nom, sans doublon.
pub fn created_volumes(config_json: &[u8], app: &str) -> Vec<CreatedVolume> {
    let Ok(root) = serde_json::from_slice::<Value>(config_json) else {
        return Vec::new();
    };
    let mut volumes = BTreeMap::new();
    for (key, volume) in root
        .get("volumes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if volume.get("external").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let default_name = format!("{app}_{key}");
        let name = volume
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&default_name);
        // Deux clés du même nom : partagé si l'une des deux le dit.
        let shared = volumes.entry(name.to_owned()).or_insert(false);
        *shared |= !own_name(app, key, name);
    }
    volumes
        .into_iter()
        .map(|(name, shared)| CreatedVolume { name, shared })
        .collect()
}

/// Classe un chemin de la machine, d'après son texte (barres répétées réduites).
pub fn host_path_kind(path: &str) -> HostPathKind {
    let path = collapse_slashes(path);
    if in_agent_dirs(&path) {
        HostPathKind::AgentDir
    } else if is_docker_socket(&path) {
        HostPathKind::EngineSocket
    } else if is_system_path(&path) {
        HostPathKind::SystemDir
    } else {
        HostPathKind::Plain
    }
}

/// Les chemins de la machine que monte la forme résolue d'un compose, hors du dossier du projet :
/// sources des montages `bind`, `file` des `configs` et des `secrets`, `device` des volumes. Pour
/// l'agent, qui regarde où ils mènent sur la machine. Sans doublon, triés.
pub fn host_sources(config_json: &[u8], project_dir: &str) -> Vec<HostSource> {
    let Ok(root) = serde_json::from_slice::<Value>(config_json) else {
        return Vec::new();
    };
    let mut sources: BTreeMap<String, bool> = BTreeMap::new();
    let mut add = |path: &str, writable: bool| {
        *sources.entry(collapse_slashes(path)).or_insert(false) |= writable;
    };
    let entries = |section: &str| {
        root.get(section)
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|entries| entries.values())
    };
    for service in entries("services") {
        for mount in service
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if mount.get("type").and_then(Value::as_str) == Some("bind")
                && let Some(source) = mount.get("source").and_then(Value::as_str)
            {
                let read_only = mount.get("read_only").and_then(Value::as_bool) == Some(true);
                add(source, !read_only);
            }
        }
    }
    for section in ["configs", "secrets"] {
        for entry in entries(section) {
            if let Some(file) = entry.get("file").and_then(Value::as_str) {
                add(file, false);
            }
        }
    }
    for volume in entries("volumes") {
        if let Some(device) = volume
            .get("driver_opts")
            .and_then(|options| options.get("device"))
            .and_then(Value::as_str)
        {
            add(device, true);
        }
    }
    sources
        .into_iter()
        .filter(|(path, _)| path.starts_with('/') && !is_within(path, project_dir))
        .map(|(path, writable)| HostSource { path, writable })
        .collect()
}

/// Analyse la sortie JSON de `docker compose config`. Rend les permissions et les
/// avertissements, ou la première règle enfreinte, dans l'ordre de docs/PROTOCOLE.md.
pub fn analyze(
    config_json: &[u8],
    context: &ComposeContext<'_>,
) -> Result<ComposeReport, StackError> {
    let root: Value =
        serde_json::from_slice(config_json).map_err(|error| invalid(error.to_string()))?;
    let root = object(&root, "configuration")?;
    let services = object(
        root.get("services")
            .ok_or_else(|| invalid("services absent"))?,
        "services",
    )?;
    check_fields(root, &[TOP_LEVEL_FIELDS], "")?;
    check_names(root)?;

    if !services.contains_key(context.service) {
        return Err(StackError::ServiceMissing(context.service.to_owned()));
    }
    if let Some(pre_update) = context.pre_update
        && !services.contains_key(pre_update)
    {
        return Err(StackError::PreUpdateMissing(pre_update.to_owned()));
    }

    let mut analysis = Analysis {
        context,
        permissions: BTreeMap::new(),
        warnings: BTreeSet::new(),
        used_images: BTreeSet::new(),
    };
    for name in sorted_keys(services) {
        let service = object(&services[name], "service")?;
        analysis.check_image(name, service)?;
        analysis.check_profiles(name, service)?;
        analysis.check_volumes(name, service)?;
        check_fields(
            service,
            &[PLAIN_SERVICE_FIELDS, CHECKED_SERVICE_FIELDS],
            &join_path("services", name),
        )?;
        analysis.collect_service(name, service)?;
    }
    if let Some(unused) = context
        .images
        .keys()
        .find(|image| !analysis.used_images.contains(*image))
    {
        return Err(StackError::ImageUnused(unused.clone()));
    }
    analysis.check_file_sources(root, "configs")?;
    analysis.check_file_sources(root, "secrets")?;
    analysis.collect_top_level(root)?;
    // Tout ce qui sera montré à la personne qui accepte : les valeurs des permissions viennent
    // de champs que la stack écrit librement (un port, un chemin, une capacité…).
    for permission in analysis.permissions.values() {
        let field = if permission.service.is_empty() {
            permission.kind.as_str().to_owned()
        } else {
            format!(
                "services.{} ({})",
                permission.service,
                permission.kind.as_str()
            )
        };
        check_visible(&field, &permission.value)?;
        if let Some(detail) = &permission.detail {
            check_visible(&field, detail)?;
        }
    }
    for warning in &analysis.warnings {
        check_visible(
            &format!("services.{}.{}", warning.service, warning.kind),
            &warning.value,
        )?;
    }

    Ok(ComposeReport {
        permissions: analysis.permissions.into_values().collect(),
        warnings: analysis.warnings.into_iter().collect(),
    })
}

/// Valeur JSON lue sans tolérer deux fois la même clé dans un objet : serde_json garderait la
/// dernière, et Compose, qui lit le même texte comme du YAML, pas forcément.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("une valeur JSON")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(Strict(value)) = items.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            let Strict(value) = entries.next_value()?;
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("clé « {key} » en double")));
            }
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

/// Un `env_file` ou un `label_file` du compose d'une archive : un chemin relatif écrit en toutes
/// lettres, dans `files/`. Ni variable, ni `\`, ni composant `.` ou `..`, sauf un `./` initial.
fn is_archived_file(path: &str) -> bool {
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.contains(['$', '\\']) {
        return false;
    }
    let mut components = path.split('/');
    let in_files = components.next() == Some(FILES_DIR);
    let rest: Vec<&str> = components.collect();
    in_files
        && !rest.is_empty()
        && rest
            .iter()
            .all(|component| !matches!(*component, "" | "." | ".."))
}

/// Première ligne du compose que publiait l'outil jusqu'à la 0.5.1. Les archives déjà publiées
/// la portent : elle est admise à l'octet près, et l'outil ne l'écrit plus.
pub const LEGACY_PUBLISHED_HEADER: &str = "# Compose publié par drakbase release : images épinglées par empreinte, sans section build. Les réglages de la machine vont dans son fichier .env, jamais ici.\n";

/// Caractères que YAML tient pour une fin de ligne, ou ignore en tête de fichier, et que JSON ne
/// connaît pas hors de `\n` : avec eux, Compose et ce contrôle ne liraient pas le même compose.
const YAML_ONLY_BREAKS: [char; 5] = ['\r', '\u{85}', '\u{2028}', '\u{2029}', '\u{feff}'];

/// Contrôle le `compose.yaml` d'une archive tel qu'il est écrit : le compose publié par l'outil
/// (docs/PROTOCOLE.md, « Compose de l'archive »). La forme résolue ne dit plus ce que Compose a lu
/// pour la produire : un `env_file` y est devenu des variables, un `extends` ou un `include` le
/// contenu d'un autre fichier.
///
/// C'est exactement un document JSON, un objet, sans commentaire ni clé en double : Compose lit
/// ce texte comme du YAML, et rien de ce que YAML comprendrait autrement n'est admis. Il n'a ni
/// `include` ni `extends`, et ses `env_file` et `label_file` sont dans `files/`.
pub fn check_archived(compose_yaml: &[u8]) -> Result<(), StackError> {
    let not_json = |what: String| StackError::ComposeNotJson(what);
    let text = std::str::from_utf8(compose_yaml)
        .map_err(|_| not_json("le texte n'est pas en UTF-8".to_owned()))?;
    let json = text.strip_prefix(LEGACY_PUBLISHED_HEADER).unwrap_or(text);
    if let Some(found) = json.chars().find(|c| YAML_ONLY_BREAKS.contains(c)) {
        return Err(not_json(format!(
            "caractère U+{:04X} : seul le saut de ligne sépare les lignes",
            u32::from(found)
        )));
    }
    if !json.trim_start_matches([' ', '\t', '\n']).starts_with('{') {
        return Err(not_json(
            "un objet JSON est attendu dès le premier caractère, sans commentaire".to_owned(),
        ));
    }
    let Strict(root) = serde_json::from_str(json).map_err(|error| not_json(error.to_string()))?;
    let root = root
        .as_object()
        .ok_or_else(|| not_json("un objet est attendu".to_owned()))?;
    let present = |map: &Map<String, Value>, field: &str| {
        map.get(field).is_some_and(|value| !value.is_null())
    };
    if present(root, "include") {
        return Err(StackError::ComposeFieldUnsupported("include".to_owned()));
    }
    let Some(services) = root.get("services").and_then(Value::as_object) else {
        return Ok(());
    };
    for name in sorted_keys(services) {
        let Some(service) = services[name].as_object() else {
            continue;
        };
        if present(service, "extends") {
            return Err(StackError::ComposeFieldUnsupported(join_path(
                &join_path("services", name),
                "extends",
            )));
        }
        for field in ["env_file", "label_file"] {
            for file in file_paths(service.get(field), field)? {
                if !is_archived_file(file) {
                    return Err(StackError::EnvFileOutsideFiles(file.to_owned()));
                }
            }
        }
    }
    Ok(())
}
