//! `drakbase sign <fichier> --app <app> --version <v>` : écrit `<fichier>.sig` au format Tauri,
//! avec la version canonique dans le commentaire de confiance.

use std::env::{self, VarError};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use am_protocol::Version;
use am_protocol::signature::{KeyId, PublicKey, Signature, decode_tauri};
use am_protocol::signing::{self, SigningError};
use anyhow::{Context, Result, bail};

use crate::credentials::Secrets;
use crate::files;
use crate::home::{AppSlug, Home, HomeArgs, KeyRole};
use crate::password::{PasswordSource, Terminal};

/// Clé secrète : son contenu, ou le chemin de son fichier, comme pour `tauri build`.
pub const KEY_VAR: &str = "TAURI_SIGNING_PRIVATE_KEY";
/// Mot de passe de la clé secrète.
pub const PASSWORD_VAR: &str = "TAURI_SIGNING_PRIVATE_KEY_PASSWORD";

#[derive(clap::Args)]
pub struct Args {
    /// Fichier à signer, par exemple Translate-1.2.0.app.tar.gz
    #[arg(value_name = "FICHIER")]
    file: PathBuf,

    /// App du fichier : sa clé principale sert à défaut de --key et de TAURI_SIGNING_PRIVATE_KEY
    #[arg(long, value_name = "APP")]
    app: AppSlug,

    /// Version du fichier, signée sous sa forme canonique (1.2 devient 1.2.0)
    #[arg(long, value_name = "VERSION")]
    version: String,

    /// Fichier de la clé secrète, au format Tauri, à la place de TAURI_SIGNING_PRIVATE_KEY et
    /// de la clé de l'app
    #[arg(long, value_name = "FICHIER")]
    key: Option<PathBuf>,

    #[command(flatten)]
    home: HomeArgs,
}

/// Clé secrète au format Tauri, et d'où elle vient, pour les messages.
struct SecretKey {
    encoded: String,
    origin: String,
    /// Identifiant de la clé, lu dans le fichier `.pub` voisin de la clé secrète : il désigne son
    /// mot de passe dans le trousseau.
    key_id: Option<KeyId>,
}

/// D'où vient le mot de passe de la clé, pour dire quoi faire s'il ne l'ouvre pas.
enum PasswordOrigin {
    /// Par la variable de Tauri, ou au terminal.
    Given,
    /// Le trousseau, ou le fichier des essais.
    Stored(String),
}

/// Une signature écrite à côté du fichier.
pub(crate) struct Signed {
    pub(crate) sig_path: PathBuf,
    pub(crate) file_name: String,
    pub(crate) key_id: KeyId,
    /// D'où vient la clé secrète : `--key`, la variable de Tauri ou le dossier des clés.
    pub(crate) origin: String,
    pub(crate) role: AppKey,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = &args.home.resolve()?;
    let version = Version::parse_release(&args.version)?;
    let secrets = Secrets::select(home)?;
    let signed = sign_file(
        &args.file,
        &args.app,
        &version,
        args.key.as_deref(),
        home,
        &secrets,
    )?;

    println!("Signature écrite : {}", signed.sig_path.display());
    println!("  fichier : {}", signed.file_name);
    let typed = args.version.trim();
    if typed == version.to_string() {
        println!("  version : {version}");
    } else {
        println!("  version : {version} (forme canonique de « {typed} »)");
    }
    println!("  clé     : {}", describe_key(&signed, &args.app));
    println!("  lue dans : {}", signed.origin);
    warn_if_foreign(&signed, &args.app, home);
    Ok(ExitCode::SUCCESS)
}

/// Signe `file` pour `app` et `version`, et écrit `<file>.sig`. La clé est celle de `key`, sinon
/// de `TAURI_SIGNING_PRIVATE_KEY`, sinon la clé principale de l'app ; le mot de passe est celui de
/// `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, sinon celui que gardent les `secrets` pour cette clé,
/// sinon une saisie au terminal.
pub(crate) fn sign_file(
    file: &Path,
    app: &AppSlug,
    version: &Version,
    key: Option<&Path>,
    home: &Home,
    secrets: &Secrets,
) -> Result<Signed> {
    let mut signed = sign_files(&[file], app, version, key, home, secrets)?;
    signed.pop().context("aucun fichier signé")
}

/// Comme [`sign_file`], pour plusieurs fichiers de la même version : la clé et son mot de passe
/// ne sont lus qu'une fois.
pub(crate) fn sign_files(
    files: &[&Path],
    app: &AppSlug,
    version: &Version,
    key: Option<&Path>,
    home: &Home,
    secrets: &Secrets,
) -> Result<Vec<Signed>> {
    let mut opened = Vec::new();
    for file in files {
        let file_name = file
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("{} : nom de fichier illisible", file.display()))?
            .to_owned();
        let data = File::open(file).with_context(|| format!("ouverture de {}", file.display()))?;
        if !data.metadata()?.is_file() {
            bail!("{} n'est pas un fichier", file.display());
        }
        opened.push((*file, file_name, data));
    }

    let key = secret_key(key, app, home)?;
    let (password, from) = read_password(&key, secrets)?;
    opened
        .into_iter()
        .map(|(file, file_name, data)| {
            sign_opened(
                file, file_name, data, &key, &password, &from, app, version, home,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn sign_opened(
    file: &Path,
    file_name: String,
    data: File,
    key: &SecretKey,
    password: &str,
    from: &PasswordOrigin,
    app: &AppSlug,
    version: &Version,
    home: &Home,
) -> Result<Signed> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("horloge du Mac antérieure à 1970")?
        .as_secs();
    let signature = match signing::sign(
        &key.encoded,
        password,
        BufReader::new(data),
        &file_name,
        version,
        timestamp,
    ) {
        Err(SigningError::SecretKey) => match from {
            PasswordOrigin::Stored(store) => bail!(
                "le mot de passe que garde {store} n'ouvre pas la clé {} : « drakbase restore {app} » réinstalle la clé et son mot de passe depuis KeePassXC",
                key.origin
            ),
            PasswordOrigin::Given => bail!(
                "impossible d'ouvrir la clé {} : mot de passe faux, ou clé illisible",
                key.origin
            ),
        },
        result => result.with_context(|| format!("signature de {}", file.display()))?,
    };
    let key_id = Signature::from_tauri(&signature)?.key_id();
    let sig_path = files::with_suffix(file, ".sig");
    files::replace(&sig_path, signature.as_bytes())?;
    Ok(Signed {
        sig_path,
        file_name,
        key_id,
        origin: key.origin.clone(),
        role: app_key_role(home, app, key_id),
    })
}

/// « 0123456789ABCDEF, clé principale de l'app translate », ou l'identifiant seul.
pub(crate) fn describe_key(signed: &Signed, app: &AppSlug) -> String {
    match signed.role {
        AppKey::Role(role) => format!("{}, {} de l'app {app}", signed.key_id, role.label()),
        AppKey::Unknown | AppKey::Foreign => signed.key_id.to_string(),
    }
}

/// Avertit si la clé employée n'est aucune de celles de l'app rangées dans le dossier des clés.
pub(crate) fn warn_if_foreign(signed: &Signed, app: &AppSlug, home: &Home) {
    if let AppKey::Foreign = signed.role {
        eprintln!(
            "attention : la clé {} n'est ni la clé principale ni la clé de secours de l'app {app} rangées dans {}. Le serveur refusera ce fichier si cette clé n'est pas déclarée pour cette app.",
            signed.key_id,
            home.keys_dir().display()
        );
    }
}

/// La clé de `--key`, sinon celle de `TAURI_SIGNING_PRIVATE_KEY`, sinon la clé principale de l'app.
fn secret_key(key: Option<&Path>, app: &AppSlug, home: &Home) -> Result<SecretKey> {
    if let Some(path) = key {
        return read_secret_key(path, path.display().to_string());
    }
    match env::var(KEY_VAR) {
        Ok(value) if !value.trim().is_empty() => {
            // Comme `tauri build` : le chemin d'un fichier existant, sinon la clé elle-même.
            let value = value.trim();
            let path = Path::new(value);
            if path.exists() {
                return read_secret_key(path, format!("{KEY_VAR} ({})", path.display()));
            }
            return checked(SecretKey {
                encoded: value.to_owned(),
                origin: KEY_VAR.to_owned(),
                key_id: None,
            });
        }
        Err(VarError::NotUnicode(_)) => bail!("{KEY_VAR} n'est pas en UTF-8"),
        _ => {}
    }
    let path = home.key_files(app, KeyRole::Main).secret;
    if !path.exists() {
        bail!(
            "aucune clé pour signer l'app {app} : ni --key, ni {KEY_VAR}, ni {}. Crée la paire de clés avec « drakbase keygen {app} ».",
            path.display()
        );
    }
    read_secret_key(&path, path.display().to_string())
}

fn read_secret_key(path: &Path, origin: String) -> Result<SecretKey> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("lecture de la clé {}", path.display()))?;
    let key_id = fs::read_to_string(files::with_suffix(path, ".pub"))
        .ok()
        .and_then(|public| PublicKey::from_tauri(public.trim()).ok())
        .map(|public| public.key_id());
    checked(SecretKey {
        encoded: text.trim().to_owned(),
        origin,
        key_id,
    })
}

/// Vrai pour une clé secrète au format Tauri, chiffrée ou non, sans rien déchiffrer.
pub(crate) fn is_secret_key(encoded: &str) -> bool {
    first_comment(encoded).is_some_and(|comment| {
        comment.starts_with("untrusted comment:") && comment.contains("secret key")
    })
}

fn first_comment(encoded: &str) -> Option<String> {
    decode_tauri(encoded)
        .ok()
        .and_then(|text| text.lines().next().map(str::to_owned))
}

/// Repère une clé publique ou un texte quelconque avant de demander le mot de passe.
fn checked(key: SecretKey) -> Result<SecretKey> {
    let comment = first_comment(&key.encoded).unwrap_or_default();
    if is_secret_key(&key.encoded) {
        Ok(key)
    } else if comment.contains("public key") {
        bail!(
            "{} est une clé publique : il faut la clé secrète (fichier .key)",
            key.origin
        )
    } else {
        bail!(
            "{} n'est pas une clé secrète au format Tauri (base64 d'une clé minisign)",
            key.origin
        )
    }
}

/// `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, même vide comme chez Tauri, sinon le mot de passe que
/// garde le trousseau pour cette clé, sinon une saisie au terminal.
fn read_password(key: &SecretKey, secrets: &Secrets) -> Result<(String, PasswordOrigin)> {
    match env::var(PASSWORD_VAR) {
        Ok(password) => return Ok((password, PasswordOrigin::Given)),
        Err(VarError::NotUnicode(_)) => bail!("{PASSWORD_VAR} n'est pas en UTF-8"),
        Err(VarError::NotPresent) => {}
    }
    if let Some(id) = key.key_id
        && let Some(password) = secrets.key_password(id)?
    {
        return Ok((
            password,
            PasswordOrigin::Stored(secrets.key_passwords_place()),
        ));
    }
    let password = Terminal {
        secret: "le mot de passe",
        otherwise: "définis TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
    }
    .read(&format!("Mot de passe de la clé {} : ", key.origin))?;
    Ok((password, PasswordOrigin::Given))
}

pub(crate) enum AppKey {
    /// Aucune clé publique de l'app dans le dossier des clés : rien à comparer.
    Unknown,
    Role(KeyRole),
    /// L'app a des clés, mais pas celle-ci.
    Foreign,
}

/// Compare la clé employée à celles de l'app rangées dans le dossier des clés : avec
/// `TAURI_SIGNING_PRIVATE_KEY` défini pour une autre app, on signerait sans s'en rendre compte
/// avec la clé de celle-ci.
fn app_key_role(home: &Home, app: &AppSlug, key_id: KeyId) -> AppKey {
    let mut known = false;
    for role in KeyRole::ALL {
        let public = fs::read_to_string(home.key_files(app, role).public)
            .ok()
            .and_then(|text| PublicKey::from_tauri(&text).ok());
        if let Some(public) = public {
            if public.key_id() == key_id {
                return AppKey::Role(role);
            }
            known = true;
        }
    }
    if known {
        AppKey::Foreign
    } else {
        AppKey::Unknown
    }
}
