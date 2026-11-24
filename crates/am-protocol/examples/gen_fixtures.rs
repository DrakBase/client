//! Régénère les jeux d'essai communs à Rust et Swift dans `fixtures/` :
//!
//!     cargo run -p am-protocol --example gen_fixtures
//!
//! Les clés d'essai existantes sont reprises telles quelles : régénérer ne change que ce qui dépend
//! du code. Leur mot de passe est public (voir `fixtures/README.md`). Ne sont pas touchés ici
//! `fixtures/versions.json`, écrit à la main, `fixtures/tauri/`, produit par le vrai Tauri, ni,
//! dans `fixtures/stack/`, les cas écrits à la main (`manifests.json`, `compose.json`, `oci.json`,
//! `control.json`) et les objets réels (`compose/`, `oci/`). Seules les archives d'essai y sont
//! générées.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use am_protocol::Version;
use am_protocol::signature::{PublicKey, decode_tauri, encode_tauri};
use am_protocol::signing::{generate_keypair, sign};
use am_protocol::stack::archive;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blake2::{Blake2b512, Digest};
use serde_json::{Value, json};

const PASSWORD: &str = "appsmarket-fixtures";
/// Date fixe du commentaire de confiance : 2026-01-01T00:00:00Z.
const TIMESTAMP: u64 = 1_767_225_600;
const UNTRUSTED_COMMENT: &str = "signature from tauri secret key";
/// Résultat de BLAKE2b-512("abc") donné par la RFC 7693, annexe A.
const RFC7693_ABC: &str = "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Octets déterministes : l'octet `i` vaut `i % 251`.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Paire de clés d'essai `keys/<name>.pub` et `keys/<name>.secret`, créée seulement si elle manque.
fn keypair(name: &str) -> Result<(String, String)> {
    let public = root().join(format!("keys/{name}.pub"));
    let secret = root().join(format!("keys/{name}.secret"));
    if public.exists() && secret.exists() {
        return Ok((fs::read_to_string(public)?, fs::read_to_string(secret)?));
    }
    let pair = generate_keypair(PASSWORD)?;
    fs::write(&public, &pair.public)?;
    fs::write(&secret, &pair.secret)?;
    Ok((pair.public, pair.secret))
}

/// Fichier créé seulement s'il manque. minisign ajoute de l'aléa à chaque signature : sans cela,
/// régénérer réécrirait des signatures valides avec d'autres octets.
fn kept(relative: &str, make: impl FnOnce() -> Result<String>) -> Result<String> {
    let path = root().join(relative);
    if path.exists() {
        return Ok(fs::read_to_string(path)?);
    }
    let content = make()?;
    fs::write(path, &content)?;
    Ok(content)
}

fn key_id(public: &str) -> Result<String> {
    Ok(PublicKey::from_tauri(public)?.key_id().to_hex())
}

/// Écrit un texte minisign dans son enveloppe Tauri.
fn write_tauri(relative: &str, text: &str) -> Result<()> {
    fs::write(root().join(relative), encode_tauri(text))?;
    Ok(())
}

/// Deuxième ligne (la ligne de données) d'un texte minisign.
fn data_line(text: &str) -> &str {
    text.lines().nth(1).unwrap_or_default()
}

/// Signe avec un commentaire de confiance arbitraire, pour les cas qui sortent du format normal.
fn sign_with_comment(secret: &str, data: &[u8], trusted_comment: &str) -> Result<String> {
    let secret_key = minisign::SecretKeyBox::from_string(&decode_tauri(secret)?)?
        .into_secret_key(Some(PASSWORD.to_owned()))?;
    let signature = minisign::sign(
        None,
        &secret_key,
        data,
        Some(trusted_comment),
        Some(UNTRUSTED_COMMENT),
    )?;
    Ok(encode_tauri(&signature.into_string()))
}

/// Remplace la ligne de données (la deuxième ligne) d'une signature Tauri après l'avoir modifiée.
fn with_data_line(signature: &str, edit: impl FnOnce(&mut Vec<u8>)) -> Result<String> {
    let text = decode_tauri(signature)?;
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut bytes = STANDARD.decode(&lines[1])?;
    edit(&mut bytes);
    lines[1] = STANDARD.encode(&bytes);
    Ok(encode_tauri(&(lines.join("\n") + "\n")))
}

fn write_json(name: &str, value: &Value) -> Result<()> {
    fs::write(
        root().join(name),
        serde_json::to_string_pretty(value)? + "\n",
    )?;
    Ok(())
}

/// Cas de vérification complète : signature, puis version annoncée.
fn case(
    name: &str,
    key: &str,
    signature: &str,
    file: &str,
    announced: &str,
    expected: &str,
) -> Value {
    json!({
        "name": name,
        "key": key,
        "file": file,
        "signature": signature,
        "announced_version": announced,
        "expected": expected,
    })
}

/// Cas de lecture d'une clé (`public_key`) ou d'une signature (`signature`) : `ok`, avec
/// l'identifiant de clé attendu, ou `malformed`.
fn parsing(name: &str, kind: &str, file: &str, key_id: Option<&str>) -> Value {
    let mut value = json!({
        "name": name,
        "kind": kind,
        "file": file,
        "expected": if key_id.is_some() { "ok" } else { "malformed" },
    });
    if let Some(id) = key_id {
        value["key_id"] = json!(id);
    }
    value
}

fn main() -> Result<()> {
    let root = root();
    fs::create_dir_all(root.join("keys"))?;
    fs::create_dir_all(root.join("files"))?;

    // Clés d'essai : celle de l'app et une autre, pour le cas « mauvaise clé ».
    let (app_public, app_secret) = keypair("test-app")?;
    let (other_public, other_secret) = keypair("other-app")?;
    let app_id = key_id(&app_public)?;
    let other_id = key_id(&other_public)?;

    // 65 537 octets : franchit une frontière de 64 Kio et plusieurs blocs BLAKE2b de 128 octets.
    let payload = pattern(65_537);
    fs::write(root.join("files/payload.bin"), &payload)?;
    let mut tampered = payload.clone();
    tampered[1000] ^= 0x01;
    fs::write(root.join("files/payload-tampered.bin"), &tampered)?;

    let version = Version::parse_release("1.2.0")?;
    let good = kept("files/payload.bin.sig", || {
        Ok(sign(
            &app_secret,
            PASSWORD,
            payload.as_slice(),
            "payload.bin",
            &version,
            TIMESTAMP,
        )?)
    })?;
    kept("files/payload.bin.other-key.sig", || {
        Ok(sign(
            &other_secret,
            PASSWORD,
            payload.as_slice(),
            "payload.bin",
            &version,
            TIMESTAMP,
        )?)
    })?;
    kept("files/payload.bin.no-version.sig", || {
        sign_with_comment(
            &app_secret,
            &payload,
            &format!("timestamp:{TIMESTAMP}\tfile:payload.bin"),
        )
    })?;
    // Même signature, algorithme annoncé « Ed » (historique) : doit être refusée avant tout calcul.
    let legacy = with_data_line(&good, |bytes| bytes[1] = b'd')?;
    fs::write(root.join("files/payload.bin.legacy-alg.sig"), &legacy)?;
    // Commentaire de confiance modifié sans la clé : la signature globale ne correspond plus.
    let tampered_comment =
        encode_tauri(&decode_tauri(&good)?.replace("version:1.2.0", "version:9.9.9"));
    fs::write(
        root.join("files/payload.bin.tampered-comment.sig"),
        &tampered_comment,
    )?;

    // Lecture des textes minisign (« Signatures » dans docs/PROTOCOLE.md). Une ligne vide en tête,
    // puis la ligne de test-app et celle d'other-app : un lecteur qui sauterait les lignes vides
    // prendrait l'identifiant sur l'une et la clé sur l'autre.
    let app_text = decode_tauri(&app_public)?;
    let other_text = decode_tauri(&other_public)?;
    let good_text = decode_tauri(&good)?;
    write_tauri(
        "keys/malformed-empty-first-line.pub",
        &format!("\n{}\n{}\n", data_line(&app_text), data_line(&other_text)),
    )?;
    write_tauri(
        "keys/malformed-no-comment.pub",
        &format!("{}\n{}\n", data_line(&app_text), data_line(&app_text)),
    )?;
    write_tauri("keys/test-app-crlf.pub", &app_text.replace('\n', "\r\n"))?;
    write_tauri(
        "files/payload.bin.empty-first-line.sig",
        &format!("\n{good_text}"),
    )?;
    write_tauri(
        "files/payload.bin.crlf.sig",
        &good_text.replace('\n', "\r\n"),
    )?;

    // Fichiers produits par le vrai Tauri (fixtures/tauri/README.md), sur payload.bin.
    let tauri_id = key_id(&fs::read_to_string(root.join("tauri/tauri-generated.pub"))?)?;
    let compat_id = key_id(&fs::read_to_string(
        root.join("tauri/appsmarket-generated.pub"),
    )?)?;

    let app_key = "keys/test-app.pub";
    let payload_file = "files/payload.bin";
    write_json(
        "signatures.json",
        &json!({
            "password": PASSWORD,
            "expected_codes": ["ok", "legacy", "key_id_mismatch", "invalid", "missing_version", "version_mismatch"],
            "cases": [
                case("valide", app_key, "files/payload.bin.sig", payload_file, "1.2.0", "ok"),
                case("version annoncée différente", app_key, "files/payload.bin.sig", payload_file, "1.3.0", "version_mismatch"),
                case("signée par une autre clé", app_key, "files/payload.bin.other-key.sig", payload_file, "1.2.0", "key_id_mismatch"),
                case("fichier modifié", app_key, "files/payload.bin.sig", "files/payload-tampered.bin", "1.2.0", "invalid"),
                case("commentaire de confiance modifié", app_key, "files/payload.bin.tampered-comment.sig", payload_file, "9.9.9", "invalid"),
                case("sans version signée", app_key, "files/payload.bin.no-version.sig", payload_file, "1.2.0", "missing_version"),
                case("format historique", app_key, "files/payload.bin.legacy-alg.sig", payload_file, "1.2.0", "legacy"),
                case("fins de ligne Windows", "keys/test-app-crlf.pub", "files/payload.bin.crlf.sig", payload_file, "1.2.0", "ok"),
                case("signée par Tauri", "tauri/tauri-generated.pub", "tauri/payload.bin.tauri.sig", payload_file, "1.2.0", "ok"),
                case("signée par Tauri sans --app-version", "tauri/tauri-generated.pub", "tauri/payload.bin.tauri-no-version.sig", payload_file, "1.2.0", "missing_version"),
                case("clé d'appsmarket, signée par Tauri", "tauri/appsmarket-generated.pub", "tauri/payload.bin.tauri-with-appsmarket-key.sig", payload_file, "1.2.0", "ok"),
                case("signature de Tauri, autre clé", app_key, "tauri/payload.bin.tauri.sig", payload_file, "1.2.0", "key_id_mismatch"),
            ],
            "parsing_codes": ["ok", "malformed"],
            "parsing": [
                parsing("clé valide", "public_key", app_key, Some(&app_id)),
                parsing("autre clé valide", "public_key", "keys/other-app.pub", Some(&other_id)),
                parsing("clé en fins de ligne Windows", "public_key", "keys/test-app-crlf.pub", Some(&app_id)),
                parsing("clé décalée par une ligne vide", "public_key", "keys/malformed-empty-first-line.pub", None),
                parsing("clé sans commentaire non signé", "public_key", "keys/malformed-no-comment.pub", None),
                parsing("clé produite par Tauri", "public_key", "tauri/tauri-generated.pub", Some(&tauri_id)),
                parsing("clé produite par appsmarket keygen", "public_key", "tauri/appsmarket-generated.pub", Some(&compat_id)),
                parsing("signature valide", "signature", "files/payload.bin.sig", Some(&app_id)),
                parsing("signature en fins de ligne Windows", "signature", "files/payload.bin.crlf.sig", Some(&app_id)),
                parsing("signature décalée par une ligne vide", "signature", "files/payload.bin.empty-first-line.sig", None),
                parsing("signature produite par Tauri", "signature", "tauri/payload.bin.tauri.sig", Some(&tauri_id)),
            ],
        }),
    )?;

    let abc = hex::encode(Blake2b512::digest(b"abc"));
    assert_eq!(
        abc, RFC7693_ABC,
        "la crate blake2 ne donne pas le vecteur de la RFC 7693"
    );
    let mut vectors = vec![json!({ "text": "abc", "digest": abc })];
    for length in [
        0usize, 1, 63, 64, 65, 127, 128, 129, 255, 256, 257, 1000, 4096, 65_537,
    ] {
        vectors.push(json!({ "pattern_length": length, "digest": hex::encode(Blake2b512::digest(pattern(length))) }));
    }
    write_json(
        "blake2b.json",
        &json!({ "algorithm": "BLAKE2b-512", "pattern": "l'octet i vaut i % 251", "vectors": vectors }),
    )?;

    stack_archives(&app_public, &app_secret)?;

    println!("Jeux d'essai régénérés dans {}", root.display());
    println!("Clé de l'app : {app_id}, autre clé : {other_id}, clé Tauri : {tauri_id}");
    Ok(())
}

/// Une entrée tar écrite à la main, pour les archives que `archive::write` refuserait de produire :
/// le nom est copié tel quel dans l'en-tête, sans aucun contrôle.
fn raw_entry(name: &[u8], entry_type: tar::EntryType, data: &[u8], link: Option<&str>) -> Vec<u8> {
    let mut header = tar::Header::new_ustar();
    header.as_old_mut().name = [0; 100];
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_entry_type(entry_type);
    header.set_size(data.len() as u64);
    header.set_mode(if entry_type.is_dir() { 0o755 } else { 0o644 });
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    if let Some(link) = link {
        header.set_link_name(link).expect("nom de lien");
    }
    header.set_cksum();
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(data);
    bytes.resize(bytes.len().div_ceil(512) * 512, 0);
    bytes
}

/// Archive gzip (niveau 9, sans nom ni date) d'entrées écrites à la main.
fn raw_archive(entries: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut tar_bytes: Vec<u8> = entries.concat();
    tar_bytes.extend_from_slice(&[0; 1024]);
    gzip(&tar_bytes)
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::GzBuilder::new().write(Vec::new(), flate2::Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn file_entry(name: &str, data: &[u8]) -> Vec<u8> {
    raw_entry(name.as_bytes(), tar::EntryType::Regular, data, None)
}

/// Archives `stack.tar.gz` d'essai (`fixtures/stack/archives/`, cas dans `archives.json`) et une
/// archive publiée et signée, `stack/demo-1.0.0-aarch64.stack.tar.gz` (sa signature est gardée).
fn stack_archives(app_public: &str, app_secret: &str) -> Result<()> {
    let dir = root().join("stack/archives");
    fs::create_dir_all(&dir)?;
    let stack_json = format!(
        "{{\n  \"format\": 1,\n  \"app\": \"demo\",\n  \"version\": \"1.0.0\",\n  \"service\": \"app\",\n  \"public_keys\": [\"{}\"],\n  \"images\": {{ \"app\": \"{DEMO_INDEX}\" }}\n}}\n",
        app_public.trim()
    );
    // Le compose d'une archive est du JSON, comme l'outil le publie (docs/PROTOCOLE.md, « Compose »).
    let mut compose = serde_json::to_string_pretty(&json!({
        "services": {
            "app": {
                "image": format!("maj.example.fr/demo/app@{DEMO_INDEX}"),
                "ports": ["${PORT:-8080}:8080"],
                "volumes": ["./files/app.conf:/etc/app.conf:ro", "data:/data"],
            }
        },
        "volumes": { "data": {} },
    }))?;
    compose.push('\n');
    let minimal = BTreeMap::from([
        ("stack.json".to_owned(), stack_json.clone().into_bytes()),
        ("compose.yaml".to_owned(), compose.clone().into_bytes()),
    ]);
    let mut full = minimal.clone();
    full.insert(
        ".env.example".to_owned(),
        b"# Port de l'interface\nPORT=8080\n".to_vec(),
    );
    full.insert("files/app.conf".to_owned(), b"listen = 8080\n".to_vec());
    full.insert("files/extra/notes.txt".to_owned(), b"rien\n".to_vec());

    let base = || {
        vec![
            file_entry("stack.json", stack_json.as_bytes()),
            file_entry("compose.yaml", compose.as_bytes()),
        ]
    };
    let with = |extra: Vec<u8>| -> Vec<Vec<u8>> {
        let mut entries = base();
        entries.push(extra);
        entries
    };
    // Exactement `MAX_ENTRIES` entrées, celle du dossier `files/` comprise : `archive::write`
    // l'écrit toujours, et une archive acceptée doit pouvoir être réécrite.
    let at_limit: Vec<Vec<u8>> = base()
        .into_iter()
        .chain([raw_entry(b"files/", tar::EntryType::Directory, b"", None)])
        .chain((0..archive::MAX_ENTRIES - 3).map(|i| file_entry(&format!("files/f{i:04}"), b"")))
        .collect();
    assert_eq!(at_limit.len(), archive::MAX_ENTRIES);

    let mut cases = Vec::new();
    let mut add = |name: &str, file: &str, bytes: Vec<u8>, expected: &str| -> Result<()> {
        fs::write(dir.join(file), bytes)?;
        cases.push(
            json!({ "name": name, "file": format!("stack/archives/{file}"), "expected": expected }),
        );
        Ok(())
    };
    add(
        "minimale, écrite par archive::write",
        "minimal.stack.tar.gz",
        archive::write(&minimal)?,
        "ok",
    )?;
    add(
        "complète, écrite par archive::write",
        "full.stack.tar.gz",
        archive::write(&full)?,
        "ok",
    )?;
    add(
        "sans entrée de dossier pour files/",
        "implicit-dirs.stack.tar.gz",
        raw_archive(&with(file_entry("files/app.conf", b"x")))?,
        "ok",
    )?;
    add(
        "sans compose.yaml",
        "missing-compose.stack.tar.gz",
        raw_archive(&[file_entry("stack.json", stack_json.as_bytes())])?,
        "entry_missing",
    )?;
    add(
        "lien symbolique",
        "symlink.stack.tar.gz",
        raw_archive(&with(raw_entry(
            b"files/passwd",
            tar::EntryType::Symlink,
            b"",
            Some("/etc/passwd"),
        )))?,
        "entry_type",
    )?;
    add(
        "lien physique",
        "hardlink.stack.tar.gz",
        raw_archive(&with(raw_entry(
            b"files/copy",
            tar::EntryType::Link,
            b"",
            Some("stack.json"),
        )))?,
        "entry_type",
    )?;
    add(
        "périphérique",
        "device.stack.tar.gz",
        raw_archive(&with(raw_entry(
            b"files/null",
            tar::EntryType::Char,
            b"",
            None,
        )))?,
        "entry_type",
    )?;
    add(
        "chemin qui remonte",
        "dotdot.stack.tar.gz",
        raw_archive(&with(file_entry("files/../../etc/cron.d/x", b"x")))?,
        "entry_path",
    )?;
    add(
        "chemin absolu",
        "absolute.stack.tar.gz",
        raw_archive(&with(file_entry("/etc/passwd", b"x")))?,
        "entry_path",
    )?;
    add(
        "chemin en ./",
        "dot-slash.stack.tar.gz",
        raw_archive(&[
            file_entry("./stack.json", stack_json.as_bytes()),
            file_entry("./compose.yaml", compose.as_bytes()),
        ])?,
        "entry_path",
    )?;
    add(
        "fichier AppleDouble",
        "appledouble.stack.tar.gz",
        raw_archive(&with(file_entry("files/._app.conf", b"x")))?,
        "entry_path",
    )?;
    add(
        "barre oblique inverse",
        "backslash.stack.tar.gz",
        raw_archive(&with(file_entry("files/a\\b", b"x")))?,
        "entry_path",
    )?;
    add(
        "nom qui n'est pas de l'UTF-8",
        "not-utf8.stack.tar.gz",
        raw_archive(&with(raw_entry(
            b"files/\xff.conf",
            tar::EntryType::Regular,
            b"x",
            None,
        )))?,
        "entry_path",
    )?;
    add(
        "fichier inattendu à la racine",
        "readme.stack.tar.gz",
        raw_archive(&with(file_entry("README.md", b"x")))?,
        "entry_unexpected",
    )?;
    add(
        "files en fichier",
        "files-as-file.stack.tar.gz",
        raw_archive(&with(file_entry("files", b"x")))?,
        "entry_unexpected",
    )?;
    add(
        "stack.json en dossier",
        "stack-json-dir.stack.tar.gz",
        raw_archive(&[
            raw_entry(b"stack.json/", tar::EntryType::Directory, b"", None),
            file_entry("compose.yaml", compose.as_bytes()),
        ])?,
        "entry_unexpected",
    )?;
    add(
        "compose.yaml deux fois",
        "duplicate.stack.tar.gz",
        raw_archive(&with(file_entry("compose.yaml", b"services: {}\n")))?,
        "entry_duplicate",
    )?;
    add(
        "fichier puis dossier du même nom",
        "file-then-dir.stack.tar.gz",
        raw_archive(
            &[
                base(),
                vec![file_entry("files/a", b"x"), file_entry("files/a/b", b"y")],
            ]
            .concat(),
        )?,
        "entry_duplicate",
    )?;
    add(
        "1000 entrées, la limite",
        "max-entries.stack.tar.gz",
        raw_archive(&at_limit)?,
        "ok",
    )?;
    add(
        "1001 entrées",
        "too-many.stack.tar.gz",
        raw_archive(&[at_limit, vec![file_entry("files/f-last", b"")]].concat())?,
        "archive_too_many_entries",
    )?;
    add(
        "plus de 16 Mio décompressée",
        "too-large.stack.tar.gz",
        raw_archive(&with(file_entry(
            "files/zeros",
            &vec![0; archive::MAX_TAR_BYTES],
        )))?,
        "archive_too_large",
    )?;
    add(
        "pas du gzip",
        "not-gzip.stack.tar.gz",
        stack_json.clone().into_bytes(),
        "archive_invalid",
    )?;
    add(
        "gzip d'autre chose qu'un tar",
        "not-tar.stack.tar.gz",
        gzip(&pattern(4096))?,
        "archive_invalid",
    )?;
    write_json(
        "stack/archives.json",
        &json!({
            "description": "Lecture des archives stack.tar.gz (docs/PROTOCOLE.md, « Archive stack.tar.gz »). expected : ok ou le code de StackError. Fichiers générés par gen_fixtures.",
            "cases": cases,
        }),
    )?;

    // Archive publiée de « demo » en 1.0.0, signée par la clé d'essai : la signature est gardée,
    // l'archive est réécrite (elle est reproductible ; si elle changeait, le test le dirait).
    let published = archive::write(&full)?;
    let name = "demo-1.0.0-aarch64.stack.tar.gz";
    fs::write(root().join("stack").join(name), &published)?;
    let version = Version::parse_release("1.0.0")?;
    kept(&format!("stack/{name}.sig"), || {
        Ok(sign(
            app_secret,
            PASSWORD,
            published.as_slice(),
            name,
            &version,
            TIMESTAMP,
        )?)
    })?;
    Ok(())
}

/// Index réel de l'image de Home Manager (essai S0), dont `stack/oci/` garde une copie.
const DEMO_INDEX: &str = "sha256:d812a9365b5761555d8517333f84fe8606f1f4ac82d37ac17476c55b902df23a";
