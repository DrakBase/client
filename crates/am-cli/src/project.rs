//! Le projet d'une app, dans le dossier courant : une app Tauri (`tauri.conf.json`), une app
//! Swift (`Package.swift` ou projet Xcode, avec son Info.plist), une stack Docker (dossier
//! `stack/`) ou une app `cli` (`drakbase.json`). `init` y écrit la configuration des mises à jour
//! et `doctor` la contrôle : les deux lisent ici ce que le projet en dit.

use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::Version;
use am_protocol::signature::{KeyId, PublicKey};
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::cli_project::CliProject;
use crate::files;
use crate::home::AppSlug;
use crate::stack::StackProject;
use crate::tauri;

/// Le dépôt public des clients de DrakBase : les apps y prennent le paquet Swift DrakBase et
/// l'aide Tauri, et les développeurs l'outil, par HTTPS, sans compte ni accès à demander. Il est
/// publié depuis le dépôt privé à chaque version (scripts/publish-client.sh).
pub const REPOSITORY_URL: &str = "https://github.com/DrakBase/client.git";
/// Le dépôt sur GitHub, sans l'hôte.
pub const REPOSITORY_PATH: &str = "DrakBase/client";
/// Les dépôts d'où les apps prenaient les clients avant le dépôt public : le dépôt privé, sous son
/// premier nom, puis sous son nom dans l'organisation DrakBase. GitHub redirige le premier vers le
/// second, qui reste privé. `doctor` signale une app qui les cite, et `init` remplace l'adresse
/// dans le `package.json` d'une app Tauri.
pub const LEGACY_REPOSITORY_PATHS: [&str; 2] =
    ["kcampion/drakbase-apps-market", "DrakBase/apps-market"];
/// Identité SwiftPM du dépôt, tirée de son adresse : `.product(…, package: …)`.
pub const SWIFT_PACKAGE: &str = "client";
/// Les identités tirées des anciennes adresses, la plus longue d'abord : un manifeste qui en cite
/// une prend encore le paquet à un ancien dépôt, par son adresse, par `package:`, ou les deux.
pub const LEGACY_SWIFT_PACKAGES: [&str; 2] = ["drakbase-apps-market", "apps-market"];
/// Le produit SwiftPM, et le module que l'app importe (`import DrakBase`).
pub const SWIFT_PRODUCT: &str = "DrakBase";
/// Son nom jusqu'à la 0.2 : encore reconnu, mais signalé.
pub const LEGACY_SWIFT_PRODUCT: &str = "AppsMarketKit";
/// Version des clients qu'installe l'outil : la sienne, qui sort sous la même étiquette git.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version minimale du plugin de Tauri : `requireSignedVersion` y apparaît.
pub const TAURI_UPDATER_MINIMUM: (u64, u64) = (2, 14);
/// Plugins Rust d'une app Tauri, épinglés comme dans l'app d'exemple : `tauri build` refuse un
/// paquet npm d'une autre version mineure que sa crate (clients/tauri/README.md, « Versions »).
pub const TAURI_CRATES: [(&str, &str); 4] = [
    ("tauri-plugin-updater", "=2.14.0"),
    ("tauri-plugin-process", "=2.4.0"),
    ("tauri-plugin-store", "=2.5.0"),
    // La version du système, que l'aide envoie au serveur.
    ("tauri-plugin-os", "=2.4.0"),
];
/// Leur pendant JavaScript, aux mêmes versions.
pub const TAURI_PACKAGES: [(&str, &str); 4] = [
    ("@tauri-apps/plugin-updater", "2.14.0"),
    ("@tauri-apps/plugin-process", "2.4.0"),
    ("@tauri-apps/plugin-store", "2.5.0"),
    ("@tauri-apps/plugin-os", "2.4.0"),
];
/// Permissions de la fenêtre qui gère les mises à jour.
pub const TAURI_PERMISSIONS: [&str; 3] =
    ["updater:default", "process:allow-restart", "store:default"];
/// Le paquet npm de l'aide.
pub const HELPER_PACKAGE: &str = "@drakbase/tauri";
/// Son nom jusqu'à la 0.2 : encore reconnu, mais signalé.
pub const LEGACY_HELPER_PACKAGE: &str = "@appsmarket/tauri";
/// Les sections d'un `package.json` où une app déclare l'aide et les plugins.
const NPM_SECTIONS: [&str; 2] = ["dependencies", "devDependencies"];
/// Ce qui précède la plage de versions d'une dépendance npm prise par l'adresse d'un dépôt git :
/// `git+ssh://…#semver:^0.5.1`.
const SEMVER_MARK: &str = "#semver:";

/// La dépendance `name` d'un `package.json` : sa section et sa valeur.
pub fn npm_dependency<'a>(package: &'a Json, name: &str) -> Option<(&'static str, &'a Json)> {
    NPM_SECTIONS
        .iter()
        .find_map(|section| Some((*section, package.get(&[section, name])?)))
}

/// La dépendance `@drakbase/tauri` qu'écrit `init` : le dépôt, à partir de la version de l'outil.
pub fn helper_dependency() -> String {
    format!("git+{REPOSITORY_URL}{SEMVER_MARK}{}", client_range())
}

/// La plage qui part de la version de l'outil : `^0.5.1`.
pub fn client_range() -> String {
    format!("^{CLIENT_VERSION}")
}

/// L'ancien dépôt que désigne le texte, s'il en désigne un : une dépendance, un manifeste.
pub fn legacy_repository(text: &str) -> Option<&'static str> {
    LEGACY_REPOSITORY_PATHS
        .into_iter()
        .find(|path| text.contains(path))
}

/// L'ancienne identité SwiftPM que cite l'un des fichiers, s'il en cite une.
pub fn legacy_swift_package(files: &[PathBuf]) -> Option<&'static str> {
    LEGACY_SWIFT_PACKAGES
        .into_iter()
        .find(|name| any_file_contains(files, name))
}

/// L'aide déclarée par l'adresse d'un dépôt, avec une plage de versions : `…#semver:^0.4.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperRange<'a> {
    /// L'adresse, jusqu'à `#semver:` compris.
    source: &'a str,
    /// La plage : `^0.4.0`.
    pub range: &'a str,
}

impl<'a> HelperRange<'a> {
    /// `None` pour une dépendance déclarée autrement : un chemin local (`file:`, `link:`),
    /// `workspace:`, une étiquette git, la version d'un registre. Celles-là ne se comparent pas à
    /// la version de l'outil.
    pub fn parse(declared: &'a str) -> Option<Self> {
        let start = declared.find(SEMVER_MARK)? + SEMVER_MARK.len();
        let (source, range) = declared.split_at(start);
        Some(Self {
            source,
            range: range.trim(),
        })
    }

    /// Ce que la plage dit de la version de l'outil.
    pub fn fit(&self) -> RangeFit {
        range_fit(self.range, CLIENT_VERSION)
    }

    /// L'aide est-elle prise à un ancien dépôt ?
    pub fn moved(&self) -> bool {
        legacy_repository(self.source).is_some()
    }

    /// La même dépendance, avec la plage `range`. Prise à un ancien dépôt, elle passe au dépôt
    /// public, par HTTPS : un ancien dépôt se prenait par SSH, qui demande un compte GitHub.
    /// Sinon, l'adresse reste celle que l'app a déclarée.
    pub fn with_range(&self, range: &str) -> String {
        if self.moved() {
            format!("git+{REPOSITORY_URL}{SEMVER_MARK}{range}")
        } else {
            format!("{}{range}", self.source)
        }
    }
}

/// Ce qu'une plage de versions npm dit d'une version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeFit {
    /// Elle l'admet.
    Includes,
    /// Elle n'admet que des versions plus anciennes : c'est établi, alternative par alternative.
    Older,
    /// Elle ne l'admet pas, sans s'arrêter avant elle : elle demande une version plus récente.
    Newer,
    /// Elle ne se lit pas.
    Unreadable,
}

/// Compare une plage npm à une version. La plage se lit avec la crate `semver`, alternative par
/// alternative (`||`) ; ce qui ne s'y ramène pas (`latest`) est illisible.
///
/// Une plage n'est « plus ancienne » que si chacune de ses alternatives s'arrête avant la
/// version : dans le doute, elle est « plus récente », et `init` n'y touche pas.
pub fn range_fit(range: &str, version: &str) -> RangeFit {
    let Ok(version) = semver::Version::parse(version) else {
        return RangeFit::Unreadable;
    };
    let Some(alternatives) = range
        .split("||")
        .map(npm_requirement)
        .collect::<Option<Vec<_>>>()
    else {
        return RangeFit::Unreadable;
    };
    if alternatives
        .iter()
        .any(|requirement| requirement.matches(&version))
    {
        return RangeFit::Includes;
    }
    let older = alternatives.iter().all(|requirement| {
        requirement
            .comparators
            .iter()
            .any(|comparator| stops_before(comparator, &version))
    });
    if older {
        RangeFit::Older
    } else {
        RangeFit::Newer
    }
}

/// Le comparateur écarte-t-il `version` et toutes celles qui la suivent ? `<0.5.0` et `^0.4.0`
/// s'arrêtent avant la 0.5.1 ; `>0.6.0`, `^0.6.0` et `<1.0.0` non.
fn stops_before(comparator: &semver::Comparator, version: &semver::Version) -> bool {
    let named = semver::Version {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        build: semver::BuildMetadata::EMPTY,
    };
    if comparator.matches(version) {
        return false;
    }
    match comparator.op {
        // Une borne haute : elle écarte la version parce qu'elle est en dessous d'elle.
        semver::Op::Less | semver::Op::LessEq => named <= *version,
        // Un intervalle qui part de la version nommée : il écarte la version parce qu'il finit
        // avant elle, s'il commence avant elle.
        semver::Op::Exact | semver::Op::Tilde | semver::Op::Caret | semver::Op::Wildcard => {
            named < *version
        }
        // `>`, `>=`, ou un opérateur que cette version ne connaît pas : aucune borne haute.
        _ => false,
    }
}

/// Les caractères d'un opérateur de comparaison.
fn is_operator(c: char) -> bool {
    matches!(c, '<' | '>' | '=' | '~' | '^')
}

/// Une plage npm sans `||`, pour la crate `semver`. Ses comparateurs, que npm sépare par des
/// blancs, le sont par des virgules. npm admet aussi un blanc entre l'opérateur et la version
/// (`>= 0.6.0`), un `v` devant la version (`v0.6.0`), et l'intervalle `0.6.0 - 0.7.0`, bornes
/// comprises. Une version complète sans opérateur est exacte pour npm (`0.4.0`), alors que
/// `semver` la lirait `^0.4.0`.
fn npm_requirement(range: &str) -> Option<semver::VersionReq> {
    let version = |text: &str| text.strip_prefix('v').unwrap_or(text).to_owned();
    if let Some((low, high)) = range.split_once(" - ") {
        let (low, high) = (version(low.trim()), version(high.trim()));
        return semver::VersionReq::parse(&format!(">={low}, <={high}")).ok();
    }
    let mut comparators: Vec<String> = Vec::new();
    // Un opérateur écrit seul, qui attend sa version.
    let mut pending = String::new();
    for token in range.split_whitespace() {
        if token.chars().all(is_operator) {
            if !pending.is_empty() {
                return None;
            }
            pending = token.to_owned();
            continue;
        }
        let token = format!("{pending}{token}");
        pending.clear();
        let split = token.find(|c| !is_operator(c)).unwrap_or(token.len());
        let (operator, text) = token.split_at(split);
        let text = version(text);
        if operator.is_empty() && semver::Version::parse(&text).is_ok() {
            comparators.push(format!("={text}"));
        } else {
            comparators.push(format!("{operator}{text}"));
        }
    }
    if !pending.is_empty() || comparators.is_empty() {
        return None;
    }
    semver::VersionReq::parse(&comparators.join(",")).ok()
}

/// Une clé de l'Info.plist d'une app Swift (docs/PROTOCOLE.md, « Intégration ») et son nom
/// d'avant la 0.3, que le paquet lit encore quand la nouvelle clé manque.
#[derive(Clone, Copy, Debug)]
pub struct InfoKey {
    pub name: &'static str,
    pub legacy: &'static str,
}

impl InfoKey {
    /// La valeur de la clé, sinon celle de son ancien nom, comme la lit le paquet.
    pub fn get<'a>(&self, info: &'a plist::Dictionary) -> Option<&'a plist::Value> {
        info.get(self.name).or_else(|| info.get(self.legacy))
    }

    /// Vrai si seule l'ancienne clé est là.
    pub fn only_legacy(&self, info: &plist::Dictionary) -> bool {
        !info.contains_key(self.name) && info.contains_key(self.legacy)
    }
}

/// Adresse de vérification.
pub const FEED_KEY: InfoKey = InfoKey {
    name: "DrakBaseFeedURL",
    legacy: "AppsMarketFeedURL",
};
/// Clés publiques embarquées : la principale, puis celle de secours.
pub const PUBLIC_KEYS_KEY: InfoKey = InfoKey {
    name: "DrakBasePublicKeys",
    legacy: "AppsMarketPublicKeys",
};
/// Toutes les clés du paquet, que `init` renomme quand il les trouve sous leur ancien nom.
pub const INFO_KEYS: [InfoKey; 4] = [
    FEED_KEY,
    PUBLIC_KEYS_KEY,
    InfoKey {
        name: "DrakBaseTrustedRequirements",
        legacy: "AppsMarketTrustedRequirements",
    },
    InfoKey {
        name: "DrakBaseInstallerService",
        legacy: "AppsMarketInstallerService",
    },
];

/// Dossiers jamais parcourus : dépendances, constructions, dépôts, tests.
const SKIPPED_DIRS: [&str; 10] = [
    ".build",
    ".git",
    ".swiftpm",
    "build",
    "dist",
    "DerivedData",
    "node_modules",
    "target",
    "Tests",
    "gen",
];
/// Profondeur des recherches de fichiers dans le projet.
const SEARCH_DEPTH: usize = 4;

pub enum Project {
    Tauri(TauriProject),
    Swift(SwiftProject),
    Stack(StackProject),
    Cli(CliProject),
}

/// Ce que la ligne de commande dit du projet : elle l'emporte sur la recherche.
#[derive(Clone, Copy, Default)]
pub struct Hints<'a> {
    /// `--tauri-config`.
    pub tauri_config: Option<&'a Path>,
    /// `--plist`.
    pub plist: Option<&'a Path>,
    /// `--stack <dossier>` : une stack Docker.
    pub stack: Option<&'a Path>,
    /// `--cli` : une app cli, dont `drakbase.json` est à la racine.
    pub cli: bool,
}

pub struct TauriProject {
    /// `tauri.conf.json`.
    pub config: PathBuf,
    /// Dossier de la configuration, d'ordinaire `src-tauri` : Cargo.toml, capabilities/, src/.
    pub src_tauri: PathBuf,
    /// Dossier de l'interface et de son `package.json`.
    pub root: PathBuf,
}

pub struct SwiftProject {
    pub root: PathBuf,
    pub manifest: SwiftManifest,
    /// L'Info.plist à compléter, s'il a été trouvé.
    pub plist: Option<PathBuf>,
}

pub enum SwiftManifest {
    Package(PathBuf),
    Xcode(PathBuf),
}

/// Le projet du dossier `dir` : celui que désignent `--stack` ou `--cli` ; sinon Tauri s'il a une
/// configuration Tauri, Swift s'il a un `Package.swift` ou un projet Xcode, une stack s'il a un
/// `stack/compose.yaml`, une app cli s'il a un `drakbase.json`. `None` sinon. `tauri_config` et
/// `plist` remplacent ce que la recherche trouverait.
pub fn detect(dir: &Path, hints: &Hints<'_>) -> Result<Option<Project>> {
    if let Some(stack) = hints.stack {
        return Ok(Some(Project::Stack(StackProject::at(&dir.join(stack))?)));
    }
    if hints.cli {
        return Ok(Some(Project::Cli(CliProject::at(dir))));
    }
    if let Some(project) = detect_app(dir, hints.tauri_config, hints.plist)? {
        return Ok(Some(project));
    }
    if let Some(stack) = StackProject::find(dir) {
        return Ok(Some(Project::Stack(stack)));
    }
    Ok(CliProject::find(dir).map(Project::Cli))
}

/// Une app Tauri ou Swift.
fn detect_app(
    dir: &Path,
    tauri_config: Option<&Path>,
    plist: Option<&Path>,
) -> Result<Option<Project>> {
    if let Some(config) = tauri::find_config_in(dir, tauri_config)? {
        let src_tauri = config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.to_owned());
        let root = if src_tauri
            .file_name()
            .is_some_and(|name| name == "src-tauri")
        {
            src_tauri
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dir.to_owned())
        } else {
            src_tauri.clone()
        };
        return Ok(Some(Project::Tauri(TauriProject {
            config,
            src_tauri,
            root,
        })));
    }
    let package = dir.join("Package.swift");
    let manifest = if package.is_file() {
        SwiftManifest::Package(package)
    } else {
        let mut projects: Vec<PathBuf> = fs::read_dir(readable(dir))
            .with_context(|| format!("lecture de {}", display(dir)))?
            .filter_map(|entry| entry.ok())
            .map(|entry| dir.join(entry.file_name()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "xcodeproj")
            })
            .collect();
        projects.sort();
        match projects.into_iter().next() {
            Some(project) => SwiftManifest::Xcode(project),
            None => return Ok(None),
        }
    };
    let plist = match plist {
        Some(path) => {
            if !path.is_file() {
                bail!("--plist : {} introuvable", path.display());
            }
            Some(path.to_owned())
        }
        None => {
            let candidates = find_files(dir, |name| name == "Info.plist");
            match candidates.as_slice() {
                [] => None,
                [one] => Some(one.clone()),
                several => bail!(
                    "plusieurs Info.plist dans le projet ({}) : désigne le bon avec --plist",
                    several
                        .iter()
                        .map(|path| display(path))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    Ok(Some(Project::Swift(SwiftProject {
        root: dir.to_owned(),
        manifest,
        plist,
    })))
}

/// Un chemin pour les messages, sans le « ./ » du dossier courant.
pub fn display(path: &Path) -> String {
    let shown = path.display().to_string();
    if shown.is_empty() {
        ".".to_owned()
    } else {
        shown.strip_prefix("./").map(str::to_owned).unwrap_or(shown)
    }
}

/// Les fichiers du projet dont le nom convient, hors des dossiers de dépendances, de
/// construction et des bundles (`.app`), triés.
pub fn find_files(dir: &Path, wanted: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(dir, 0, &wanted, &mut found);
    found.sort();
    found
}

/// Le dossier à lister : le dossier courant pour un chemin vide, qui garde les chemins relatifs
/// courts dans les messages.
fn readable(dir: &Path) -> &Path {
    if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    }
}

fn walk(dir: &Path, depth: usize, wanted: &impl Fn(&str) -> bool, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(readable(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = dir.join(&name);
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let skipped = SKIPPED_DIRS.contains(&name.as_str())
                || [".app", ".xcodeproj", ".xcassets", ".xcworkspace"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix));
            if !skipped && depth < SEARCH_DEPTH {
                walk(&path, depth + 1, wanted, found);
            }
        } else if wanted(&name) {
            found.push(path);
        }
    }
}

/// Vrai si l'un des fichiers contient ce texte.
pub fn any_file_contains(paths: &[PathBuf], needle: &str) -> bool {
    paths.iter().any(|path| {
        fs::read_to_string(path)
            .map(|text| text.contains(needle))
            .unwrap_or(false)
    })
}

/// Adresse de vérification d'une app Swift (`DrakBaseFeedURL`).
pub fn swift_feed(server: &str, app: &AppSlug) -> String {
    format!("{}/api/v1/update/{app}", server.trim_end_matches('/'))
}

/// Adresse de vérification d'une app Tauri (`plugins.updater.endpoints`).
pub fn tauri_endpoint(server: &str, app: &AppSlug) -> String {
    format!(
        "{}/api/v1/update/{app}/{{{{target}}}}/{{{{arch}}}}/{{{{current_version}}}}?bundle_type={{{{bundle_type}}}}",
        server.trim_end_matches('/')
    )
}

/// L'app que désigne une adresse de vérification : `…/api/v1/update/<app>[/…][?…]`.
pub fn app_of_feed(url: &str) -> Option<AppSlug> {
    let rest = url.split("/api/v1/update/").nth(1)?;
    rest.split(['/', '?']).next()?.parse().ok()
}

/// Identifiant d'une clé publique au format Tauri.
pub fn key_id(public_key: &str) -> Result<KeyId> {
    Ok(PublicKey::from_tauri(public_key.trim())
        .context("clé publique illisible (format Tauri attendu)")?
        .key_id())
}

/// Une version écrite telle quelle, ou `None` pour un gabarit (`__VERSION__`, `$(MARKETING_VERSION)`).
pub fn literal_version(text: &str) -> Option<Version> {
    Version::parse_release(text.trim()).ok()
}

/// Valeur JSON qui garde l'ordre des clés : `init` modifie `tauri.conf.json`, `package.json` et
/// les capacités sans réordonner ce que le développeur a écrit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(IndexMap<String, Json>),
}

impl Json {
    pub fn read(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("lecture de {}", display(path)))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} n'est pas un JSON valide", display(path)))
    }

    /// Réécrit le fichier, indenté de deux espaces, comme le font Tauri et npm.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        files::replace(path, text.as_bytes())
    }

    /// La valeur au bout de ces clés.
    pub fn get(&self, keys: &[&str]) -> Option<&Json> {
        keys.iter().try_fold(self, |value, key| match value {
            Json::Object(map) => map.get(*key),
            _ => None,
        })
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Range `value` au bout de ces clés, en créant les objets qui manquent. Vrai si quelque
    /// chose a changé. Refuse de traverser une valeur qui n'est pas un objet.
    pub fn set(&mut self, keys: &[&str], value: Json) -> Result<bool> {
        let Some((last, path)) = keys.split_last() else {
            bail!("chemin vide");
        };
        let mut current = self;
        for key in path {
            let Json::Object(map) = current else {
                bail!("{} n'est pas un objet", keys.join("."));
            };
            current = map
                .entry((*key).to_owned())
                .or_insert_with(|| Json::Object(IndexMap::new()));
        }
        let Json::Object(map) = current else {
            bail!("{} n'est pas un objet", path.join("."));
        };
        if map.get(*last) == Some(&value) {
            return Ok(false);
        }
        map.insert((*last).to_owned(), value);
        Ok(true)
    }

    pub fn string(text: &str) -> Json {
        Json::String(text.to_owned())
    }
}

/// Les dépendances déclarées dans le texte d'un Cargo.toml, par leur nom, quelle que soit leur
/// forme (`nom = "1.0"`, `nom = { … }`, `nom.workspace = true`).
pub fn cargo_declares(text: &str, name: &str) -> bool {
    cargo_line(text, name).is_some()
}

fn cargo_line<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find(|line| {
        line.trim_start()
            .strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with(['=', '.']))
    })
}

/// La version déclarée d'une dépendance (`"2.14.0"`, `"=2.14.0"`, `{ version = "^2.14" }`),
/// sous la forme (majeure, mineure).
pub fn cargo_version(text: &str, name: &str) -> Option<(u64, u64)> {
    let line = cargo_line(text, name)?;
    let after = line.split_once('=')?.1;
    let quoted = match after.find("version") {
        Some(index) => &after[index..],
        None => after,
    };
    let version = quoted
        .split('"')
        .nth(1)?
        .trim_start_matches(['=', '^', '~', ' ']);
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// La version du paquet d'un Cargo.toml (`version = "…"` de `[package]`), si elle y est écrite.
pub fn cargo_package_version(text: &str) -> Option<Version> {
    let mut in_package = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package
            && let Some(value) = line
                .strip_prefix("version")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            return literal_version(value.trim().trim_matches('"'));
        }
    }
    None
}

/// Ajoute à la section `[dependencies]` les crates qui n'y sont pas déjà, juste sous son
/// en-tête ; crée la section si elle manque. Rend le texte et les crates ajoutées.
pub fn add_cargo_dependencies(text: &str, crates: &[(&str, &str)]) -> (String, Vec<String>) {
    let missing: Vec<(&str, &str)> = crates
        .iter()
        .copied()
        .filter(|(name, _)| !cargo_declares(text, name))
        .collect();
    if missing.is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let lines: String = missing
        .iter()
        .map(|(name, version)| format!("{name} = \"{version}\"\n"))
        .collect();
    let added = missing.iter().map(|(name, _)| (*name).to_owned()).collect();
    let mut result = String::with_capacity(text.len() + lines.len());
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        result.push_str(line);
        if !inserted && line.trim() == "[dependencies]" {
            if !line.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&lines);
            inserted = true;
        }
    }
    if !inserted {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("\n[dependencies]\n");
        result.push_str(&lines);
    }
    (result, added)
}

/// Une variable collée à un caractère non ASCII, que le bash 3.2 de macOS lit de travers dans une
/// locale UTF-8 : `$APP…` y devient la variable `APP…`. Rend les numéros de ligne et les noms.
pub fn glued_variables(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut position = 0;
        while position < bytes.len() {
            if bytes[position] == b'$'
                && bytes
                    .get(position + 1)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            {
                let start = position + 1;
                let mut end = start;
                while bytes
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    end += 1;
                }
                if bytes.get(end).is_some_and(|byte| !byte.is_ascii()) {
                    found.push((index + 1, line[start..end].to_owned()));
                }
                position = end;
            } else {
                position += 1;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(text: &str) -> AppSlug {
        text.parse().unwrap()
    }

    #[test]
    fn stacks_and_cli_apps_are_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let none = Hints::default();
        fs::write(dir.path().join("drakbase.json"), "{}").unwrap();
        assert!(matches!(
            detect(dir.path(), &none).unwrap(),
            Some(Project::Cli(_))
        ));
        fs::create_dir_all(dir.path().join("stack")).unwrap();
        fs::write(dir.path().join("stack/compose.yaml"), "services: {}\n").unwrap();
        match detect(dir.path(), &none).unwrap() {
            Some(Project::Stack(stack)) => assert_eq!(stack.dir, dir.path().join("stack")),
            _ => panic!("stack attendue"),
        }
        // Les options l'emportent sur la recherche.
        let cli = Hints {
            cli: true,
            ..Hints::default()
        };
        assert!(matches!(
            detect(dir.path(), &cli).unwrap(),
            Some(Project::Cli(_))
        ));
        let elsewhere = Hints {
            stack: Some(Path::new("deploy")),
            ..Hints::default()
        };
        let error = detect(dir.path(), &elsewhere).err().unwrap().to_string();
        assert!(error.contains("deploy/compose.yaml introuvable"), "{error}");
        // Une app Tauri ou Swift passe avant une stack.
        fs::write(dir.path().join("Package.swift"), "// swift\n").unwrap();
        assert!(matches!(
            detect(dir.path(), &none).unwrap(),
            Some(Project::Swift(_))
        ));
    }

    #[test]
    fn projects_are_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let none = Hints::default();
        assert!(detect(dir.path(), &none).unwrap().is_none());

        fs::write(
            dir.path().join("Package.swift"),
            "// swift-tools-version: 6.0\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("scripts")).unwrap();
        fs::write(dir.path().join("scripts/Info.plist"), "<plist/>").unwrap();
        // Un bundle construit n'est pas un Info.plist du projet.
        fs::create_dir_all(dir.path().join("dist/Demo.app/Contents")).unwrap();
        fs::write(
            dir.path().join("dist/Demo.app/Contents/Info.plist"),
            "<plist/>",
        )
        .unwrap();
        match detect(dir.path(), &none).unwrap() {
            Some(Project::Swift(project)) => {
                assert!(matches!(project.manifest, SwiftManifest::Package(_)));
                assert_eq!(project.plist, Some(dir.path().join("scripts/Info.plist")));
            }
            _ => panic!("app Swift attendue"),
        }

        fs::create_dir_all(dir.path().join("Resources")).unwrap();
        fs::write(dir.path().join("Resources/Info.plist"), "<plist/>").unwrap();
        let several = detect(dir.path(), &none).err().unwrap().to_string();
        assert!(several.contains("--plist"), "{several}");

        fs::create_dir_all(dir.path().join("src-tauri")).unwrap();
        fs::write(dir.path().join("src-tauri/tauri.conf.json"), "{}").unwrap();
        match detect(dir.path(), &none).unwrap() {
            Some(Project::Tauri(project)) => {
                assert_eq!(project.config, dir.path().join("src-tauri/tauri.conf.json"));
                assert_eq!(project.src_tauri, dir.path().join("src-tauri"));
                assert_eq!(project.root, dir.path());
            }
            _ => panic!("app Tauri attendue"),
        }
    }

    #[test]
    fn feed_addresses_name_the_app() {
        let app = slug("mon-app");
        let feed = swift_feed("https://updates.example.fr/", &app);
        assert_eq!(feed, "https://updates.example.fr/api/v1/update/mon-app");
        let endpoint = tauri_endpoint("https://updates.example.fr", &app);
        assert_eq!(
            endpoint,
            "https://updates.example.fr/api/v1/update/mon-app/{{target}}/{{arch}}/{{current_version}}?bundle_type={{bundle_type}}"
        );
        assert_eq!(app_of_feed(&feed), Some(app.clone()));
        assert_eq!(app_of_feed(&endpoint), Some(app));
        assert_eq!(app_of_feed("https://updates.example.fr/"), None);
    }

    #[test]
    fn json_edits_keep_the_order_and_say_what_changed() {
        let mut json: Json = serde_json::from_str(
            r#"{"$schema": "s", "productName": "Démo", "version": "0.1.0", "bundle": {"active": true}, "nul": null, "n": 1.5}"#,
        )
        .unwrap();
        assert!(
            json.set(&["plugins", "updater", "pubkey"], Json::string("clé"))
                .unwrap()
        );
        assert!(
            json.set(&["bundle", "createUpdaterArtifacts"], Json::Bool(true))
                .unwrap()
        );
        assert!(
            !json
                .set(&["bundle", "createUpdaterArtifacts"], Json::Bool(true))
                .unwrap()
        );
        assert!(json.set(&["version", "x"], Json::Null).is_err());
        let text = serde_json::to_string(&json).unwrap();
        assert_eq!(
            text,
            r#"{"$schema":"s","productName":"Démo","version":"0.1.0","bundle":{"active":true,"createUpdaterArtifacts":true},"nul":null,"n":1.5,"plugins":{"updater":{"pubkey":"clé"}}}"#
        );
        assert_eq!(
            json.get(&["plugins", "updater", "pubkey"])
                .and_then(Json::as_str),
            Some("clé")
        );
    }

    #[test]
    fn cargo_dependencies_are_added_once() {
        let text = "[package]\nname = \"demo\"\n\n[dependencies]\ntauri = { version = \"2\" }\ntauri-plugin-store.workspace = true\n";
        let (updated, added) = add_cargo_dependencies(text, &TAURI_CRATES);
        assert_eq!(
            added,
            [
                "tauri-plugin-updater",
                "tauri-plugin-process",
                "tauri-plugin-os"
            ]
        );
        assert!(updated.contains("[dependencies]\ntauri-plugin-updater = \"=2.14.0\"\ntauri-plugin-process = \"=2.4.0\"\ntauri-plugin-os = \"=2.4.0\"\ntauri = "));
        let (again, none) = add_cargo_dependencies(&updated, &TAURI_CRATES);
        assert!(none.is_empty());
        assert_eq!(again, updated);
        assert_eq!(
            cargo_version(&updated, "tauri-plugin-updater"),
            Some((2, 14))
        );
        assert_eq!(
            cargo_version("x = { version = \"=2.13.1\", features = [] }", "x"),
            Some((2, 13))
        );
        assert_eq!(cargo_version("x.workspace = true", "x"), None);

        let package = "[package]\nname = \"demo\"\nversion = \"0.3.1\"\n\n[dependencies]\nversion-check = \"1\"\n";
        assert_eq!(cargo_package_version(package), literal_version("0.3.1"));
        assert_eq!(
            cargo_package_version("[package]\nversion.workspace = true\n"),
            None
        );

        let (created, added) =
            add_cargo_dependencies("[package]\nname = \"demo\"", &TAURI_CRATES[..1]);
        assert_eq!(added, ["tauri-plugin-updater"]);
        assert_eq!(
            created,
            "[package]\nname = \"demo\"\n\n[dependencies]\ntauri-plugin-updater = \"=2.14.0\"\n"
        );
    }

    /// Les manifestes de la racine, par lesquels les apps installent les clients, suivent la version
    /// de l'outil, l'aide Tauri et les modules du paquet Swift ; les plugins qu'ajoute `init` sont
    /// ceux de l'app d'exemple.
    #[test]
    fn root_manifests_follow_the_clients() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let package = Json::read(&root.join("package.json")).unwrap();
        assert_eq!(
            package.get(&["version"]).and_then(Json::as_str),
            Some(CLIENT_VERSION)
        );
        let helper = Json::read(&root.join("clients/tauri/package.json")).unwrap();
        assert_eq!(
            helper.get(&["name"]).and_then(Json::as_str),
            Some(HELPER_PACKAGE)
        );
        for key in ["name", "version", "peerDependencies"] {
            assert_eq!(package.get(&[key]), helper.get(&[key]), "{key}");
        }
        for (name, version) in TAURI_PACKAGES {
            let pinned = helper
                .get(&["devDependencies", name])
                .and_then(Json::as_str);
            assert_eq!(pinned, Some(version), "{name}");
        }
        let example =
            fs::read_to_string(root.join("examples/tauri-demo/src-tauri/Cargo.toml")).unwrap();
        for (name, version) in TAURI_CRATES {
            let line = cargo_line(&example, name).unwrap_or_else(|| panic!("{name}"));
            assert!(line.contains(&format!("\"{version}\"")), "{line}");
        }

        let manifest = fs::read_to_string(root.join("Package.swift")).unwrap();
        for module in fs::read_dir(root.join("clients/swift/DrakBase/Sources")).unwrap() {
            let name = module.unwrap().file_name().into_string().unwrap();
            let path = format!("\"\\(sources)/{name}\"");
            assert!(manifest.contains(&path), "{name} absent de Package.swift");
        }

        // Les versions que cite un fichier, toutes celle de l'outil ; rend leur nombre.
        let citations = |path: &Path| -> usize {
            let text = fs::read_to_string(path).unwrap();
            let mut cited = 0;
            for marker in [
                "--tag v",
                "from: \"",
                // La dépendance à écrire, avec l'adresse du dépôt. Une plage citée seule
                // (`#semver:^0.4.0`) est l'exemple d'une app à monter.
                ".git#semver:^",
                "version **",
                "à partir de la version ",
                "TOOL_VERSION = \"",
                "étiquettes du dépôt (`v",
            ] {
                for (index, _) in text.match_indices(marker) {
                    let version: String = text[index + marker.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    // `--tag v${TOOL_VERSION}` : la version vient de la constante, contrôlée aussi.
                    if version.is_empty() {
                        continue;
                    }
                    assert_eq!(
                        version,
                        CLIENT_VERSION,
                        "{} : {marker}{version}",
                        path.display()
                    );
                    cited += 1;
                }
            }
            cited
        };

        // La skill Claude (clients/claude, publiée telle quelle dans un dépôt public, dont elle est
        // à la fois la place de marché et le seul plugin), la page Développeurs de l'administration
        // et le README font installer cette même version. Ils restent dans le dépôt privé : le
        // dépôt public des clients (scripts/publish-client.sh) n'a ni la skill ni l'interface web.
        if root.join("clients/claude").is_dir() {
            let plugin =
                Json::read(&root.join("clients/claude/.claude-plugin/plugin.json")).unwrap();
            assert_eq!(
                plugin.get(&["version"]).and_then(Json::as_str),
                Some(CLIENT_VERSION)
            );
            let marketplace =
                fs::read_to_string(root.join("clients/claude/.claude-plugin/marketplace.json"))
                    .unwrap();
            assert!(marketplace.contains("\"source\": \"./\""));
            let skill = root.join("clients/claude/skills/drakbase");
            let mut texts = vec![
                skill.join("SKILL.md"),
                root.join("web/src/admin/DevelopersPage.tsx"),
                root.join("README.md"),
            ];
            for reference in fs::read_dir(skill.join("references")).unwrap() {
                texts.push(reference.unwrap().path());
            }
            let cited: usize = texts.iter().map(|path| citations(path)).sum();
            assert!(cited >= 8, "{cited} versions citées seulement");
        }

        // Le manifeste de la racine et les guides des clients, que lisent ceux qui les installent,
        // citent eux aussi la version : la dépendance à écrire, et l'étiquette du dépôt.
        for (file, expected) in [
            ("Package.swift", 1),
            ("clients/swift/DrakBase/README.md", 2),
            ("clients/tauri/README.md", 2),
        ] {
            let cited = citations(&root.join(file));
            assert!(
                cited >= expected,
                "{file} : {cited} version(s) citée(s), {expected} attendue(s) au moins"
            );
        }
    }

    #[test]
    fn helper_ranges_are_compared_to_the_version_of_the_tool() {
        for (range, expected) in [
            ("^0.5.1", RangeFit::Includes),
            ("^0.5.0", RangeFit::Includes),
            ("~0.5.0", RangeFit::Includes),
            ("0.5", RangeFit::Includes),
            ("0.5.x", RangeFit::Includes),
            ("0.x", RangeFit::Includes),
            ("*", RangeFit::Includes),
            (">=0.4.0 <0.6.0", RangeFit::Includes),
            ("^0.4.0 || ^0.5.0", RangeFit::Includes),
            ("0.5.1", RangeFit::Includes),
            // La plage qu'écrivait l'outil 0.4 : elle s'arrête avant la 0.5.
            ("^0.4.0", RangeFit::Older),
            ("~0.4.2", RangeFit::Older),
            ("0.4.x", RangeFit::Older),
            (">=0.3.0 <0.5.0", RangeFit::Older),
            ("^0.3.0 || ^0.4.0", RangeFit::Older),
            // Une version complète sans opérateur est exacte, pour npm.
            ("0.5.0", RangeFit::Older),
            ("^0.5.2", RangeFit::Newer),
            ("^0.6.0", RangeFit::Newer),
            (">=1.0.0 <2.0.0", RangeFit::Newer),
            ("^0.4.0 || ^0.6.0", RangeFit::Newer),
            ("1.0.0", RangeFit::Newer),
            // Une borne basse stricte n'admet pas la version qu'elle nomme : la plage n'en est pas
            // plus ancienne pour autant.
            (">0.6.0", RangeFit::Newer),
            (">0.5.1", RangeFit::Newer),
            (">0.5.1 <0.6.0", RangeFit::Newer),
            (">0.6", RangeFit::Newer),
            (">0.6.0-rc.1 <0.6.0", RangeFit::Newer),
            ("<0.5.0 || >0.6.0", RangeFit::Newer),
            (">0.4.0 <0.5.0", RangeFit::Older),
            ("<0.5.1", RangeFit::Older),
            ("<=0.5.0", RangeFit::Older),
            // Les écritures que npm admet en plus : un blanc après l'opérateur, un `v`, un
            // intervalle.
            (">= 0.6.0", RangeFit::Newer),
            ("> 0.4.0", RangeFit::Includes),
            (">= 0.4.0 < 0.5.0", RangeFit::Older),
            ("v0.6.0", RangeFit::Newer),
            ("v0.5.1", RangeFit::Includes),
            ("=v0.4.0", RangeFit::Older),
            ("0.6.0 - 0.7.0", RangeFit::Newer),
            ("0.4.0 - 0.5.0", RangeFit::Older),
            ("0.4.0 - 0.5", RangeFit::Includes),
            ("", RangeFit::Unreadable),
            ("latest", RangeFit::Unreadable),
            ("^0.4.0 ||", RangeFit::Unreadable),
            (">=", RangeFit::Unreadable),
            (">= <= 0.5.0", RangeFit::Unreadable),
            ("0.4.0 - ", RangeFit::Unreadable),
        ] {
            assert_eq!(range_fit(range, "0.5.1"), expected, "{range}");
        }
        assert_eq!(range_fit("^0.5.1", "pas une version"), RangeFit::Unreadable);
    }

    #[test]
    fn only_a_helper_taken_from_a_repository_with_a_range_is_compared() {
        let declared = "git+https://github.com/DrakBase/client.git#semver:^0.4.0";
        let helper = HelperRange::parse(declared).unwrap();
        assert_eq!(helper.range, "^0.4.0");
        assert!(!helper.moved());
        // La plage suit l'outil, l'adresse reste celle que l'app a déclarée.
        assert_eq!(
            helper.with_range(&client_range()),
            format!("git+https://github.com/DrakBase/client.git#semver:^{CLIENT_VERSION}")
        );
        let other = HelperRange::parse("github:DrakBase/client#semver:~0.4.1").unwrap();
        assert_eq!(
            other.with_range(&client_range()),
            format!("github:DrakBase/client#semver:^{CLIENT_VERSION}")
        );
        // Ce qu'écrit init admet la version de l'outil, et passe par HTTPS.
        let written = helper_dependency();
        assert_eq!(
            written,
            format!("git+https://github.com/DrakBase/client.git#semver:^{CLIENT_VERSION}")
        );
        let helper = HelperRange::parse(&written).unwrap();
        assert_eq!(helper.fit(), RangeFit::Includes);
        assert_eq!(helper.with_range(&client_range()), written);
        for declared in [
            "file:../drakbase/clients/tauri",
            "link:../drakbase/clients/tauri",
            "workspace:*",
            "^0.4.0",
            "git+ssh://git@github.com/kcampion/drakbase-apps-market.git#v0.4.0",
        ] {
            assert_eq!(HelperRange::parse(declared), None, "{declared}");
        }

        // Les anciens dépôts, privés : l'aide passe au dépôt public, par HTTPS, quelle que soit la
        // forme de l'adresse, avec la plage gardée ou montée.
        for old in [
            "git+ssh://git@github.com/kcampion/drakbase-apps-market.git#semver:^0.4.0",
            "github:kcampion/drakbase-apps-market#semver:^0.4.0",
            "git+ssh://git@github.com/DrakBase/apps-market.git#semver:^0.4.0",
        ] {
            let legacy = HelperRange::parse(old).unwrap();
            assert!(legacy.moved(), "{old}");
            assert_eq!(
                legacy.with_range(legacy.range),
                "git+https://github.com/DrakBase/client.git#semver:^0.4.0"
            );
            assert_eq!(legacy.with_range(&client_range()), written);
        }
        assert_eq!(
            legacy_repository(
                ".package(url: \"git@github.com:kcampion/drakbase-apps-market.git\", from: \"0.5.3\")"
            ),
            Some("kcampion/drakbase-apps-market")
        );
        assert_eq!(
            legacy_repository("git+ssh://git@github.com/DrakBase/apps-market.git"),
            Some("DrakBase/apps-market")
        );
        assert_eq!(legacy_repository(REPOSITORY_URL), None);
        assert_eq!(legacy_repository(&helper_dependency()), None);

        let package: Json = serde_json::from_str(
            r#"{"dependencies": {"a": "1"}, "devDependencies": {"a": "2", "b": "3"}}"#,
        )
        .unwrap();
        let found = |name: &str| {
            npm_dependency(&package, name)
                .map(|(section, value)| (section, value.as_str().unwrap_or_default().to_owned()))
        };
        assert_eq!(found("a"), Some(("dependencies", "1".to_owned())));
        assert_eq!(found("b"), Some(("devDependencies", "3".to_owned())));
        assert_eq!(found("c"), None);
    }

    #[test]
    fn glued_variables_are_found() {
        let script = "echo \"▸ Assemblage de $APP…\"\necho \"${APP}…\"\necho \"$VERSION, $HOME é\"\necho \"$1…\"\n";
        assert_eq!(glued_variables(script), [(1, "APP".to_owned())]);
    }

    #[test]
    fn versions_are_literal_or_templates() {
        assert!(literal_version("1.2").is_some());
        assert!(literal_version("__VERSION__").is_none());
        assert!(literal_version("$(MARKETING_VERSION)").is_none());
    }
}
