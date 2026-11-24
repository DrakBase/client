//! `drakbase verify <fichier> --pubkey <clé> [--version <v>]` : les contrôles de la signature,
//! dans l'ordre de docs/PROTOCOLE.md, avec le résultat de chacun. Le verdict est toujours celui
//! d'`am_protocol`, la même vérification que celle du serveur.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use am_protocol::Version;
use am_protocol::signature::{
    PublicKey, Signature, SignatureError, StreamVerifier, Verified, decode_tauri,
};
use anyhow::{Context, Result, anyhow};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::files;

#[derive(clap::Args)]
pub struct Args {
    /// Fichier signé
    #[arg(value_name = "FICHIER")]
    file: PathBuf,

    /// Clé publique au format Tauri, ou fichier qui la contient (par exemple translate.key.pub)
    #[arg(long, value_name = "CLÉ")]
    pubkey: String,

    /// Version attendue : la version signée doit lui être égale, une fois normalisée
    #[arg(long, value_name = "VERSION")]
    version: Option<String>,

    /// Signature au format Tauri, ou fichier qui la contient [défaut : <FICHIER>.sig]
    #[arg(long, value_name = "SIGNATURE")]
    signature: Option<String>,
}

/// Les contrôles de docs/PROTOCOLE.md, dans leur ordre obligatoire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Algorithm,
    KeyId,
    FileSignature,
    GlobalSignature,
    Version,
}

impl Check {
    const ALL: [Check; 5] = [
        Check::Algorithm,
        Check::KeyId,
        Check::FileSignature,
        Check::GlobalSignature,
        Check::Version,
    ];

    fn title(self) -> &'static str {
        match self {
            Check::Algorithm => "Algorithme",
            Check::KeyId => "Identifiant de clé",
            Check::FileSignature => "Signature du fichier (Ed25519 sur son BLAKE2b-512)",
            Check::GlobalSignature => "Signature globale (commentaire de confiance)",
            Check::Version => "Version signée",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Passed,
    Failed,
    /// Accepté, mais à signaler : une signature sans version.
    Warning,
    NotChecked,
}

struct Outcome {
    check: Check,
    status: Status,
    detail: String,
}

/// Résultat des contrôles, et le commentaire de confiance une fois authentifié.
#[derive(Default)]
struct Report {
    outcomes: Vec<Outcome>,
    verified: Option<Verified>,
}

impl Report {
    fn record(&mut self, check: Check, status: Status, detail: impl Into<String>) {
        self.outcomes.push(Outcome {
            check,
            status,
            detail: detail.into(),
        });
    }

    /// Les contrôles qui suivent un échec ne sont pas faits.
    fn stop(mut self) -> Self {
        for check in Check::ALL.into_iter().skip(self.outcomes.len()) {
            self.record(check, Status::NotChecked, "contrôle non effectué");
        }
        self
    }

    /// Une version non comparée, faute de `--version`, ne fait pas refuser la signature.
    fn accepted(&self) -> bool {
        self.outcomes
            .iter()
            .all(|outcome| outcome.status != Status::Failed)
    }
}

pub fn run(args: Args) -> Result<ExitCode> {
    let expected = args
        .version
        .as_deref()
        .map(Version::parse_release)
        .transpose()
        .context("--version")?;
    let (key_text, key_origin) = file_or_value(&args.pubkey, "clé publique")?;
    let key =
        PublicKey::from_tauri(&key_text).map_err(|_| unreadable_key(&key_text, &key_origin))?;
    let (signature_text, signature_origin) = match &args.signature {
        Some(value) => file_or_value(value, "signature")?,
        None => {
            let path = files::with_suffix(&args.file, ".sig");
            let text = fs::read_to_string(&path)
                .with_context(|| format!("lecture de la signature {}", path.display()))?;
            (text, path.display().to_string())
        }
    };
    let signature = Signature::from_tauri(&signature_text).with_context(|| {
        format!("{signature_origin} : signature illisible (format Tauri attendu)")
    })?;
    let data =
        File::open(&args.file).with_context(|| format!("ouverture de {}", args.file.display()))?;

    println!("Fichier   : {}", args.file.display());
    println!("Signature : {signature_origin}");
    println!("Clé       : {} ({key_origin})", key.key_id());
    println!();

    let report = check(
        &key,
        &signature,
        BufReader::new(data),
        expected.as_ref(),
        || failed_signature_check(&key_text, &signature_text, &args.file),
    )?;
    for (number, outcome) in report.outcomes.iter().enumerate() {
        let symbol = match outcome.status {
            Status::Passed => "✓",
            Status::Failed => "✗",
            Status::Warning => "!",
            Status::NotChecked => "·",
        };
        println!(
            "{symbol} {}. {} : {}",
            number + 1,
            outcome.check.title(),
            outcome.detail
        );
    }
    if let Some(verified) = &report.verified {
        println!();
        println!("Commentaire de confiance : {}", describe_comment(verified));
    }
    println!();
    if report.accepted() {
        if expected.is_none() {
            println!("Signature valide. Ajoute --version pour contrôler aussi la version signée.");
        } else {
            println!("Signature valide.");
        }
        Ok(ExitCode::SUCCESS)
    } else {
        println!("Signature refusée.");
        Ok(ExitCode::FAILURE)
    }
}

/// Fait les contrôles dans l'ordre et s'arrête au premier échec. `diagnose` ne sert qu'à dire
/// lequel des contrôles 3 et 4 a échoué.
fn check(
    key: &PublicKey,
    signature: &Signature,
    mut data: impl Read,
    expected: Option<&Version>,
    diagnose: impl FnOnce() -> Option<Check>,
) -> Result<Report> {
    let mut report = Report::default();
    let mut verifier = match key.verify_stream(signature) {
        Ok(verifier) => verifier,
        Err(SignatureError::Legacy) => {
            report.record(
                Check::Algorithm,
                Status::Failed,
                "Ed, format historique non préhaché, refusé",
            );
            return Ok(report.stop());
        }
        Err(SignatureError::KeyIdMismatch { expected, found }) => {
            report.record(Check::Algorithm, Status::Passed, "ED, préhaché");
            report.record(
                Check::KeyId,
                Status::Failed,
                format!("signature faite avec la clé {found}, la clé donnée est {expected}"),
            );
            return Ok(report.stop());
        }
        Err(other) => return Err(other.into()),
    };
    report.record(Check::Algorithm, Status::Passed, "ED, préhaché");
    report.record(Check::KeyId, Status::Passed, key.key_id().to_string());

    io::copy(&mut data, &mut Feed(&mut verifier)).context("lecture du fichier")?;
    let verified = match verifier.finalize() {
        Ok(verified) => verified,
        Err(SignatureError::Invalid) => {
            match diagnose() {
                Some(Check::GlobalSignature) => {
                    report.record(Check::FileSignature, Status::Passed, "valide");
                    report.record(
                        Check::GlobalSignature,
                        Status::Failed,
                        "invalide (le commentaire de confiance a été modifié depuis la signature)",
                    );
                }
                Some(Check::FileSignature) => report.record(
                    Check::FileSignature,
                    Status::Failed,
                    "invalide (le fichier a changé depuis la signature, ou ce n'est pas le fichier signé)",
                ),
                _ => report.record(
                    Check::FileSignature,
                    Status::Failed,
                    "invalide (signature du fichier ou signature globale)",
                ),
            }
            return Ok(report.stop());
        }
        Err(other) => return Err(other.into()),
    };
    report.record(Check::FileSignature, Status::Passed, "valide");
    report.record(Check::GlobalSignature, Status::Passed, "valide");

    // Le champ `version:` n'est lu qu'à présent : avant, le commentaire n'était pas authentifié.
    match (expected, verified.field("version")) {
        (Some(expected), _) => match verified.require_version(expected) {
            Ok(()) => report.record(
                Check::Version,
                Status::Passed,
                format!("{expected}, celle attendue"),
            ),
            Err(SignatureError::MissingVersion) => report.record(
                Check::Version,
                Status::Failed,
                "aucune version dans le commentaire de confiance",
            ),
            Err(SignatureError::VersionMismatch { signed, expected }) => report.record(
                Check::Version,
                Status::Failed,
                format!("{signed}, alors que {expected} est attendue"),
            ),
            Err(other) => return Err(other.into()),
        },
        (None, Some(signed)) => report.record(
            Check::Version,
            Status::NotChecked,
            format!("{signed}, non comparée (pas de --version)"),
        ),
        (None, None) => report.record(
            Check::Version,
            Status::Warning,
            "aucune, le serveur et les apps refuseront ce fichier",
        ),
    }
    report.verified = Some(verified);
    Ok(report)
}

/// Fait passer le contenu du fichier dans le vérificateur, par `io::copy`.
struct Feed<'a, 'b>(&'a mut StreamVerifier<'b>);

impl Write for Feed<'_, '_> {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        self.0.update(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `am_protocol` rend la même erreur pour les contrôles 3 et 4. Le crate minisign, qui produit
/// les signatures, les distingue, mais seulement par le texte de son erreur : on ne s'en sert
/// que pour l'affichage, jamais pour décider.
fn failed_signature_check(key: &str, signature: &str, file: &Path) -> Option<Check> {
    let key = minisign::PublicKeyBox::from_string(&decode_tauri(key).ok()?)
        .ok()?
        .into_public_key()
        .ok()?;
    let signature = minisign::SignatureBox::from_string(&decode_tauri(signature).ok()?).ok()?;
    let data = BufReader::new(File::open(file).ok()?);
    let message = minisign::verify(&key, &signature, data, true, false, false)
        .err()?
        .to_string();
    if message.contains("Comment signature verification failed") {
        Some(Check::GlobalSignature)
    } else if message.contains("Signature verification failed") {
        Some(Check::FileSignature)
    } else {
        None
    }
}

/// Le texte donné tel quel, ou le contenu du fichier qu'il désigne ; avec la façon de le citer.
fn file_or_value(value: &str, what: &str) -> Result<(String, String)> {
    let path = Path::new(value);
    if path.is_file() {
        let text = fs::read_to_string(path)
            .with_context(|| format!("lecture de la {what} {}", path.display()))?;
        Ok((text, path.display().to_string()))
    } else {
        Ok((value.to_owned(), format!("{what} donnée en argument")))
    }
}

fn unreadable_key(text: &str, origin: &str) -> anyhow::Error {
    let decoded = decode_tauri(text).unwrap_or_default();
    if decoded.contains("secret key") {
        anyhow!("{origin} est une clé secrète : donne la clé publique (fichier .key.pub)")
    } else if origin.ends_with("donnée en argument") {
        anyhow!(
            "« {} » n'est ni un fichier, ni une clé publique au format Tauri",
            text.trim()
        )
    } else {
        anyhow!("{origin} : clé publique illisible (format Tauri attendu)")
    }
}

/// Commentaire de confiance authentifié, avec la date de signature lisible.
fn describe_comment(verified: &Verified) -> String {
    let mut parts = Vec::new();
    if let Some(file) = verified.field("file") {
        parts.push(format!("fichier « {file} »"));
    }
    let signed_at = verified
        .field("timestamp")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|date| {
            date.format(format_description!(
                "[year]-[month]-[day] à [hour]:[minute]:[second] UTC"
            ))
            .ok()
        });
    if let Some(signed_at) = signed_at {
        parts.push(format!("signé le {signed_at}"));
    }
    if parts.is_empty() {
        verified.trusted_comment.replace('\t', "  ")
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use serde_json::Value;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    fn read(relative: &str) -> String {
        fs::read_to_string(fixtures().join(relative)).unwrap()
    }

    /// Rejoue les cas communs de `fixtures/signatures.json` : chacun doit échouer au bon contrôle.
    #[test]
    fn fixture_cases_fail_at_the_expected_check() {
        let cases: Value = serde_json::from_str(&read("signatures.json")).unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let field = |name: &str| case[name].as_str().unwrap();
            let key_text = read(field("key"));
            let signature_text = read(field("signature"));
            let file = fixtures().join(field("file"));
            let key = PublicKey::from_tauri(&key_text).unwrap();
            let signature = Signature::from_tauri(&signature_text).unwrap();
            let expected = Version::parse_release(field("announced_version")).unwrap();
            let report = check(
                &key,
                &signature,
                File::open(&file).unwrap(),
                Some(&expected),
                || failed_signature_check(&key_text, &signature_text, &file),
            )
            .unwrap();
            let failed: Vec<Check> = report
                .outcomes
                .iter()
                .filter(|outcome| outcome.status == Status::Failed)
                .map(|outcome| outcome.check)
                .collect();
            let wanted = match (field("expected"), field("name")) {
                ("ok", _) => vec![],
                ("legacy", _) => vec![Check::Algorithm],
                ("key_id_mismatch", _) => vec![Check::KeyId],
                ("invalid", "commentaire de confiance modifié") => vec![Check::GlobalSignature],
                ("invalid", _) => vec![Check::FileSignature],
                ("missing_version" | "version_mismatch", _) => vec![Check::Version],
                (other, _) => panic!("résultat inconnu : {other}"),
            };
            assert_eq!(failed, wanted, "cas « {} »", field("name"));
            assert_eq!(report.outcomes.len(), Check::ALL.len());
            assert_eq!(report.accepted(), wanted.is_empty());
        }
    }

    #[test]
    fn without_expected_version_the_signed_one_is_shown() {
        let key = PublicKey::from_tauri(&read("keys/test-app.pub")).unwrap();
        let signature = Signature::from_tauri(&read("files/payload.bin.sig")).unwrap();
        let data = File::open(fixtures().join("files/payload.bin")).unwrap();
        let report = check(&key, &signature, data, None, || None).unwrap();
        assert!(report.accepted());
        let last = report.outcomes.last().unwrap();
        assert_eq!(last.status, Status::NotChecked);
        assert!(last.detail.starts_with("1.2.0"), "{}", last.detail);
        assert_eq!(
            describe_comment(report.verified.as_ref().unwrap()),
            "fichier « payload.bin », signé le 2026-01-01 à 00:00:00 UTC"
        );
    }

    #[test]
    fn a_signature_without_version_is_accepted_with_a_warning() {
        let key = PublicKey::from_tauri(&read("keys/test-app.pub")).unwrap();
        let signature = Signature::from_tauri(&read("files/payload.bin.no-version.sig")).unwrap();
        let data = File::open(fixtures().join("files/payload.bin")).unwrap();
        let report = check(&key, &signature, data, None, || None).unwrap();
        assert!(report.accepted());
        assert_eq!(report.outcomes.last().unwrap().status, Status::Warning);
    }

    #[test]
    fn a_secret_key_given_as_public_key_is_named() {
        let secret = read("keys/test-app.secret");
        let message = unreadable_key(&secret, "test-app.secret").to_string();
        assert!(message.contains("clé secrète"), "{message}");
    }
}
