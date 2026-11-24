//! Aides des essais du binaire contre le faux serveur. `HOME` et `APPSMARKET_HOME` pointent
//! toujours vers un dossier temporaire, les identifiants et les mots de passe des clés vont dans
//! des fichiers (`APPSMARKET_CREDENTIALS=file`), les sauvegardes dans des bases KeePassXC jetables,
//! et aucun navigateur ne s'ouvre : aucun essai n'écrit dans le vrai trousseau ni dans ta base.
#![allow(dead_code)]

pub mod fake_server;

use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;

/// Mot de passe public des clés de `fixtures/keys`.
pub const FIXTURE_PASSWORD: &str = "appsmarket-fixtures";
/// Mot de passe des bases KeePassXC d'essai.
pub const KEEPASSXC_PASSWORD: &str = "base-essai-appsmarket";
/// Types de processeur Mach-O.
pub const CPU_ARM64: u32 = 0x0100_000c;
pub const CPU_X86_64: u32 = 0x0100_0007;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Le binaire, avec son dossier sous `home`, sans les variables de l'utilisateur qui changeraient
/// son comportement, et sans proxy.
pub fn drakbase(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_drakbase"));
    command
        .env("HOME", home)
        .env("APPSMARKET_HOME", home.join("outil"))
        .env("APPSMARKET_CREDENTIALS", "file")
        .env("APPSMARKET_BROWSER", "none")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY_PASSWORD")
        .stdin(Stdio::null());
    for proxy in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(proxy);
    }
    command
}

pub fn run(command: &mut Command) -> Output {
    command.output().unwrap()
}

pub fn run_with_stdin(command: &mut Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Lance la commande, attend que `ready` soit vrai, fait `act`, puis attend la fin de la commande :
/// de quoi jouer l'administration pendant qu'`init` attend. Les sorties sont lues pendant
/// l'attente : un tube plein bloquerait la commande avant l'étape attendue.
pub fn run_acting(
    command: &mut Command,
    input: &str,
    ready: impl Fn() -> bool,
    act: impl FnOnce(),
) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "la commande n'a pas atteint l'étape attendue"
        );
        thread::sleep(Duration::from_millis(50));
    }
    act();
    let status = child.wait().unwrap();
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

/// Lit `pipe` jusqu'au bout dans un fil à part.
fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    })
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Sortie et erreurs ensemble, pour les messages d'échec.
pub fn both(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// `drakbase login` sur le faux serveur, avec le jeton sur l'entrée standard.
pub fn login(home: &Path, server: &str, token: &str) -> Output {
    run_with_stdin(
        drakbase(home).args(["login", "--server", server, "--token-stdin"]),
        &format!("{token}\n"),
    )
}

/// Signe `file` pour `version`, avec une clé d'essai de `fixtures/keys` (`test-app` ou
/// `other-app`).
pub fn sign(home: &Path, file: &Path, version: &str, key: &str) {
    let output = run(drakbase(home)
        .env("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", FIXTURE_PASSWORD)
        .arg("sign")
        .arg(file)
        .args(["--app", "test-app", "--version", version, "--key"])
        .arg(fixtures().join(format!("keys/{key}.secret"))));
    assert!(output.status.success(), "{}", both(&output));
}

/// En-tête d'un exécutable Mach-O avec ces processeurs : simple pour un seul, universel sinon.
pub fn mach_o(cpus: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let [cpu] = cpus {
        bytes.extend([0xcf, 0xfa, 0xed, 0xfe]);
        bytes.extend(cpu.to_le_bytes());
        bytes.extend([0; 24]);
    } else {
        bytes.extend([0xca, 0xfe, 0xba, 0xbe]);
        bytes.extend((cpus.len() as u32).to_be_bytes());
        for cpu in cpus {
            bytes.extend(cpu.to_be_bytes());
            bytes.extend([0; 16]);
        }
    }
    bytes
}

/// Types d'un fichier ELF (`e_type`) : un fichier objet (`.o`), un exécutable.
pub const ELF_OBJECT: u16 = 1;
pub const ELF_EXECUTABLE: u16 = 2;
/// Machines d'un fichier ELF (`e_machine`).
pub const ELF_AARCH64: u16 = 0xb7;
pub const ELF_X86_64: u16 = 0x3e;

/// Un fichier ELF réduit à son en-tête : 64 bits, petit-boutien, de ce type (`e_type`, octets 16
/// et 17) et de cette machine (`e_machine`, octets 18 et 19).
pub fn elf(kind: u16, machine: u16) -> Vec<u8> {
    let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
    bytes.resize(16, 0);
    bytes.extend(kind.to_le_bytes());
    bytes.extend(machine.to_le_bytes());
    bytes.resize(64, 0);
    bytes
}

/// Une fausse app à empaqueter.
pub struct FakeApp<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub identifier: &'a str,
    pub cpus: &'a [u32],
    /// `AppsMarketPublicKeys`, au format Tauri.
    pub public_keys: &'a [String],
}

impl FakeApp<'_> {
    /// Écrit `<dir>/<nom>-<version>.app.tar.gz` : un seul dossier `Nom.app`, son Info.plist, et
    /// un exécutable réduit à son en-tête Mach-O, que l'outil ne fait que lire.
    pub fn archive(&self, dir: &Path) -> PathBuf {
        let path = dir.join(format!("{}-{}.app.tar.gz", self.name, self.version));
        let keys = if self.public_keys.is_empty() {
            String::new()
        } else {
            let items: String = self
                .public_keys
                .iter()
                .map(|key| format!("<string>{key}</string>"))
                .collect();
            format!("<key>AppsMarketPublicKeys</key><array>{items}</array>")
        };
        let info = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>{identifier}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
{keys}
</dict></plist>
"#,
            name = self.name,
            identifier = self.identifier,
            version = self.version,
        );
        let mut builder = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::fast(),
        ));
        let top = format!("{}.app", self.name);
        let executable = mach_o(self.cpus);
        for (name, contents, mode) in [
            (format!("{top}/Contents/Info.plist"), info.as_bytes(), 0o644),
            (
                format!("{top}/Contents/MacOS/{}", self.name),
                executable.as_slice(),
                0o755,
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(mode);
            header.set_size(contents.len() as u64);
            builder.append_data(&mut header, name, contents).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }
}

/// Clé publique d'essai de `fixtures/keys`.
pub fn public_key(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(format!("keys/{name}.pub")))
        .unwrap()
        .trim()
        .to_owned()
}

/// keepassxc-cli, comme le cherche l'outil : sans lui, les essais qui sauvegardent des clés
/// s'arrêtent après l'avoir dit.
pub fn keepassxc_cli() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = env::var_os("APPSMARKET_KEEPASSXC_CLI")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    candidates.push("/Applications/KeePassXC.app/Contents/MacOS/keepassxc-cli".into());
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|dir| dir.join("keepassxc-cli")));
    }
    let found = candidates.into_iter().find(|path| path.is_file());
    if found.is_none() {
        eprintln!("keepassxc-cli introuvable : essai sauté");
    }
    found
}

/// Une base KeePassXC jetable dans `dir`, rapide à ouvrir.
pub fn keepassxc_database(dir: &Path) -> Option<PathBuf> {
    let cli = keepassxc_cli()?;
    let path = dir.join("essai.kdbx");
    let mut child = Command::new(cli)
        .args(["db-create", "-q", "-p", "-t", "100"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{KEEPASSXC_PASSWORD}\n{KEEPASSXC_PASSWORD}\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    Some(path)
}

/// Une entrée de la base, lue par keepassxc-cli : mot de passe, nom d'utilisateur et notes.
pub struct Entry {
    pub password: String,
    pub username: String,
    pub notes: String,
}

pub fn keepassxc_entry(database: &Path, path: &str) -> Option<Entry> {
    let mut child = Command::new(keepassxc_cli()?)
        .args([
            "show", "-q", "-s", "-a", "Password", "-a", "UserName", "-a", "Notes",
        ])
        .arg(database)
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{KEEPASSXC_PASSWORD}\n").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.splitn(3, '\n');
    Some(Entry {
        password: lines.next()?.to_owned(),
        username: lines.next()?.to_owned(),
        notes: lines.next().unwrap_or_default().to_owned(),
    })
}

/// Les clés d'essai de `fixtures/keys` comme clés de l'app `test-app`, dans le dossier de l'outil
/// sous `home` : `test-app` en principale, `other-app` en secours. La principale est la clé
/// active de l'app du faux serveur.
pub fn fixture_keys(home: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let keys = home.join("outil/keys");
    fs::create_dir_all(&keys).unwrap();
    for (from, to) in [("test-app", "test-app"), ("other-app", "test-app.backup")] {
        let secret = keys.join(format!("{to}.key"));
        fs::copy(fixtures().join(format!("keys/{from}.secret")), &secret).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::copy(
            fixtures().join(format!("keys/{from}.pub")),
            keys.join(format!("{to}.key.pub")),
        )
        .unwrap();
    }
}

/// Comme après `drakbase backup test-app` : la sauvegarde notée, et le mot de passe de la clé
/// principale rangé.
pub fn fixture_keys_saved(home: &Path) {
    let id = |name: &str| {
        am_protocol::signature::PublicKey::from_tauri(&public_key(name))
            .unwrap()
            .key_id()
            .to_hex()
    };
    let (main, backup) = (id("test-app"), id("other-app"));
    fs::write(
        home.join("outil/backups.json"),
        format!(
            "{{\"test-app\": {{\"keepassxc\": \"/essai/base.kdbx\", \"saved_at\": \"2026-10-28T12:00:00Z\", \"main\": \"{main}\", \"backup\": \"{backup}\"}}}}"
        ),
    )
    .unwrap();
    fs::write(
        home.join("outil/key-passwords.json"),
        format!("{{\"{main}\": \"{FIXTURE_PASSWORD}\"}}"),
    )
    .unwrap();
}
