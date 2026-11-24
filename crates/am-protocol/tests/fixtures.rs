//! Vérifie les jeux d'essai communs (`fixtures/`) avec l'implémentation Rust. Le paquet Swift
//! rejoue les mêmes fichiers.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use am_protocol::signature::{PublicKey, Signature, SignatureError};
use am_protocol::signing;
use am_protocol::update::UpdateResponse;
use am_protocol::{OsVersion, Version};
use blake2::{Blake2b512, Digest};
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn read_json(name: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(fixtures().join(name)).unwrap()).unwrap()
}

fn read_text(relative: &str) -> String {
    fs::read_to_string(fixtures().join(relative)).unwrap()
}

fn field<'a>(case: &'a Value, name: &str) -> &'a str {
    case[name]
        .as_str()
        .unwrap_or_else(|| panic!("champ « {name} » absent"))
}

#[test]
fn version_normalization_vectors() {
    for case in read_json("versions.json")["normalize"].as_array().unwrap() {
        let input = field(case, "input");
        let release = Version::parse_release(input)
            .ok()
            .map(|version| version.to_string());
        let client = Version::parse_client(input)
            .ok()
            .map(|version| version.to_string());
        assert_eq!(
            release.as_deref(),
            case["release"].as_str(),
            "publication « {input} »"
        );
        assert_eq!(
            client.as_deref(),
            case["client"].as_str(),
            "client « {input} »"
        );
    }
}

#[test]
fn version_ordering_vectors() {
    let versions: Vec<Version> = read_json("versions.json")["ordered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| Version::parse_release(raw.as_str().unwrap()).unwrap())
        .collect();
    for pair in versions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{} devrait précéder {}",
            pair[0],
            pair[1]
        );
    }
}

fn outcome(case: &Value) -> &'static str {
    let key = PublicKey::from_tauri(&read_text(field(case, "key"))).unwrap();
    let signature = Signature::from_tauri(&read_text(field(case, "signature"))).unwrap();
    let data = fs::read(fixtures().join(field(case, "file"))).unwrap();
    let announced = Version::parse_release(field(case, "announced_version")).unwrap();
    match key
        .verify(&data, &signature)
        .and_then(|verified| verified.require_version(&announced))
    {
        Ok(()) => "ok",
        Err(SignatureError::Legacy) => "legacy",
        Err(SignatureError::KeyIdMismatch { .. }) => "key_id_mismatch",
        Err(SignatureError::Invalid) => "invalid",
        Err(SignatureError::MissingVersion) => "missing_version",
        Err(SignatureError::VersionMismatch { .. }) => "version_mismatch",
        Err(other) => panic!("résultat inattendu : {other}"),
    }
}

/// Les résultats que donnent les cas d'une section : ce sont exactement les codes que le jeu
/// d'essai annonce (`expected_codes`, `parsing_codes`), ceux que chaque implémentation distingue.
fn assert_cases_cover_codes(fixtures: &Value, section: &str, codes: &str) {
    let covered: BTreeSet<&str> = fixtures[section]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| field(case, "expected"))
        .collect();
    let announced: BTreeSet<&str> = fixtures[codes]
        .as_array()
        .unwrap()
        .iter()
        .map(|code| code.as_str().unwrap())
        .collect();
    assert_eq!(covered, announced, "{section} et {codes}");
}

#[test]
fn signature_vectors() {
    let fixtures = read_json("signatures.json");
    assert_cases_cover_codes(&fixtures, "cases", "expected_codes");
    for case in fixtures["cases"].as_array().unwrap() {
        assert_eq!(
            outcome(case),
            field(case, "expected"),
            "cas « {} »",
            field(case, "name")
        );
    }
}

#[test]
fn parsing_vectors() {
    let fixtures = read_json("signatures.json");
    assert_cases_cover_codes(&fixtures, "parsing", "parsing_codes");
    for case in fixtures["parsing"].as_array().unwrap() {
        let name = field(case, "name");
        let encoded = read_text(field(case, "file"));
        let (result, malformed) = match field(case, "kind") {
            "public_key" => (
                PublicKey::from_tauri(&encoded).map(|key| key.key_id()),
                SignatureError::MalformedPublicKey,
            ),
            "signature" => (
                Signature::from_tauri(&encoded).map(|signature| signature.key_id()),
                SignatureError::MalformedSignature,
            ),
            other => panic!("cas « {name} » : type « {other} » inconnu"),
        };
        match field(case, "expected") {
            "ok" => assert_eq!(
                result.unwrap().to_hex(),
                field(case, "key_id"),
                "cas « {name} »"
            ),
            "malformed" => assert_eq!(result.unwrap_err(), malformed, "cas « {name} »"),
            other => panic!("cas « {name} » : résultat « {other} » inconnu"),
        }
    }
}

#[test]
fn streaming_matches_in_memory_verification() {
    let key = PublicKey::from_tauri(&read_text("keys/test-app.pub")).unwrap();
    let signature = Signature::from_tauri(&read_text("files/payload.bin.sig")).unwrap();
    let data = fs::read(fixtures().join("files/payload.bin")).unwrap();
    let mut verifier = key.verify_stream(&signature).unwrap();
    for chunk in data.chunks(4096) {
        verifier.update(chunk);
    }
    let verified = verifier.finalize().unwrap();
    assert_eq!(verified.key_id, key.key_id());
    assert_eq!(verified.field("file"), Some("payload.bin"));
}

#[test]
fn sign_round_trip_with_fixture_key() {
    let secret = read_text("keys/test-app.secret");
    let key = PublicKey::from_tauri(&read_text("keys/test-app.pub")).unwrap();
    // Le mot de passe des clés d'essai, tel que le jeu d'essai le donne.
    let fixtures = read_json("signatures.json");
    let password = field(&fixtures, "password");
    let version = Version::parse_release("2.0").unwrap();
    let encoded = signing::sign(
        &secret,
        password,
        &b"contenu"[..],
        "x.app.tar.gz",
        &version,
        1,
    )
    .unwrap();
    let verified = key
        .verify(b"contenu", &Signature::from_tauri(&encoded).unwrap())
        .unwrap();
    verified.require_version(&version).unwrap();
    assert_eq!(
        signing::secret_key_id(&secret, password).unwrap(),
        key.key_id()
    );
    assert!(signing::secret_key_id(&secret, "mauvais mot de passe").is_err());
    assert!(signing::secret_key_id(&read_text("keys/test-app.pub"), password).is_err());
    assert!(signing::sign(&secret, "mauvais mot de passe", &b"x"[..], "x", &version, 1).is_err());
    assert!(signing::sign(&secret, password, &b"x"[..], "a\tb", &version, 1).is_err());
}

#[test]
fn os_version_vectors() {
    let fixtures = read_json("os-versions.json");
    let parse = |raw: &Value| OsVersion::parse(raw.as_str().unwrap());
    for case in fixtures["parse"].as_array().unwrap() {
        let input = field(case, "input");
        let found = OsVersion::parse(input).map(|version| version.components().to_vec());
        let expected: Option<Vec<u64>> = case["components"]
            .as_array()
            .map(|numbers| numbers.iter().map(|n| n.as_u64().unwrap()).collect());
        assert_eq!(found, expected, "lecture de « {input} »");
    }
    for pair in fixtures["ordered"].as_array().unwrap().windows(2) {
        let (low, high) = (parse(&pair[0]).unwrap(), parse(&pair[1]).unwrap());
        assert!(low < high, "{} devrait précéder {}", pair[0], pair[1]);
        assert!(high.at_least(&low) && !low.at_least(&high));
    }
    for pair in fixtures["equal"].as_array().unwrap() {
        assert_eq!(parse(&pair[0]), parse(&pair[1]), "{pair}");
        assert!(parse(&pair[0]).is_some(), "{pair}");
    }
    for case in fixtures["at_least"].as_array().unwrap() {
        let version = parse(&case["version"]).unwrap();
        let minimum = parse(&case["minimum"]).unwrap();
        assert_eq!(
            version.at_least(&minimum),
            case["expected"].as_bool().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn update_response_vectors() {
    for case in read_json("update-response.json")["cases"]
        .as_array()
        .unwrap()
    {
        let name = field(case, "name");
        let result = serde_json::from_value::<UpdateResponse>(case["body"].clone());
        match field(case, "expected") {
            "refused" => assert!(result.is_err(), "cas « {name} » : {result:?}"),
            "ok" => {
                let response = result.unwrap_or_else(|error| panic!("cas « {name} » : {error}"));
                // Ce qui a été lu, champ par champ, les champs absents valant `null`.
                let read = json!({
                    "version": response.version.to_string(),
                    "url": response.url,
                    "signature": response.signature,
                    "notes": response.notes,
                    "pub_date": response.pub_date,
                    "size": response.size,
                    "sha256": response.sha256,
                    "min_os_version": response.min_os_version,
                    "channel": response.channel.as_str(),
                });
                assert_eq!(read, case["check"], "cas « {name} »");
            }
            other => panic!("cas « {name} » : résultat « {other} » inconnu"),
        }
    }
}

#[test]
fn blake2b_vectors() {
    let data = read_json("blake2b.json");
    for vector in data["vectors"].as_array().unwrap() {
        let input = match vector["text"].as_str() {
            Some(text) => text.as_bytes().to_vec(),
            None => (0..vector["pattern_length"].as_u64().unwrap() as usize)
                .map(|i| (i % 251) as u8)
                .collect(),
        };
        assert_eq!(
            hex::encode(Blake2b512::digest(&input)),
            field(vector, "digest")
        );
    }
}
