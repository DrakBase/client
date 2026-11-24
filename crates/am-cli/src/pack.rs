//! `drakbase pack-macos <Nom.app> [--dmg]` : archive de mise à jour `Nom-<version>.app.tar.gz`,
//! et en option le DMG de première installation.
//!
//! L'archive a un seul dossier `Nom.app/` à sa racine, garde les liens symboliques et les droits,
//! et ne contient aucun fichier AppleDouble `._*` (docs/PROTOCOLE.md) : elle est écrite avec le
//! crate `tar`, qui n'emporte jamais les attributs étendus. Le bundle doit être signé avec un
//! certificat : une signature ad hoc ne vaut que pour le binaire exact, et aucune mise à jour ne
//! passerait le contrôle de signature de code des apps.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use anyhow::{Context, Result, bail, ensure};
use flate2::Compression;
use flate2::write::GzEncoder;
use tar::{EntryType, Header};

use crate::files;
use crate::macos::BundleInfo;

#[derive(clap::Args)]
pub struct Args {
    /// Bundle de l'app, signé avec ton certificat
    #[arg(value_name = "NOM.app")]
    bundle: PathBuf,

    /// Crée aussi le DMG de première installation, avec un raccourci vers /Applications
    #[arg(long)]
    dmg: bool,

    /// Dossier où écrire l'archive et le DMG [défaut : celui du bundle]
    #[arg(short, long, value_name = "DOSSIER")]
    output: Option<PathBuf>,

    /// Remplace l'archive et le DMG s'ils existent déjà
    #[arg(long)]
    force: bool,
}

pub fn run(args: Args) -> Result<ExitCode> {
    let bundle = read_bundle(&args.bundle)?;
    let dir = output_dir(&bundle, args.output.as_deref())?;
    let outputs = Outputs::new(&bundle, &dir, args.dmg, args.force)?;
    let authority = require_certificate(&bundle.path)?;

    print_bundle(&bundle);
    println!("Certificat  : {authority}");
    println!();

    let removed_signature = package(&bundle, &outputs, args.force)?;
    println!(
        "✓ Archive : {} ({})",
        outputs.archive.display(),
        am_protocol::human_size(fs::metadata(&outputs.archive)?.len())
    );
    println!(
        "  extraite par /usr/bin/tar : un seul dossier {}, signature de code toujours valide",
        bundle.dir_name()
    );
    if let Some(signature) = removed_signature {
        println!(
            "  ancienne signature supprimée, elle ne correspondait plus : {}",
            signature.display()
        );
    }
    if let Some(dmg) = &outputs.dmg {
        println!(
            "✓ DMG : {} ({})",
            dmg.display(),
            am_protocol::human_size(fs::metadata(dmg)?.len())
        );
    }
    println!();
    println!(
        "Étape suivante : drakbase sign {} --app <app> --version {}",
        outputs.archive.display(),
        bundle.info.version
    );
    Ok(ExitCode::SUCCESS)
}

/// Le dossier donné, créé s'il manque, sinon celui du bundle.
pub(crate) fn output_dir(bundle: &Bundle, output: Option<&Path>) -> Result<PathBuf> {
    match output {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
            Ok(dir.to_owned())
        }
        None => Ok(bundle
            .path
            .parent()
            .context("le bundle n'a pas de dossier parent")?
            .to_owned()),
    }
}

/// Chemin, identifiant, version et macOS minimal du bundle.
pub(crate) fn print_bundle(bundle: &Bundle) {
    let info = &bundle.info;
    println!("Bundle      : {}", bundle.path.display());
    if let Some(identifier) = &info.identifier {
        println!("Identifiant : {identifier}");
    }
    if info.raw_version == info.version.to_string() {
        println!("Version     : {}", info.version);
    } else {
        println!(
            "Version     : {} (forme canonique de « {} »)",
            info.version, info.raw_version
        );
    }
    if let Some(minimum) = &info.minimum_macos {
        println!("macOS       : {minimum} au minimum");
    }
}

/// Ce que l'outil lit dans le bundle.
#[derive(Debug)]
pub(crate) struct Bundle {
    pub(crate) path: PathBuf,
    /// Nom du dossier sans `.app` : `Translate` pour `Translate.app`.
    pub(crate) name: String,
    pub(crate) info: BundleInfo,
}

impl Bundle {
    fn dir_name(&self) -> String {
        format!("{}.app", self.name)
    }
}

pub(crate) fn read_bundle(path: &Path) -> Result<Bundle> {
    let path = fs::canonicalize(path).with_context(|| format!("bundle {}", path.display()))?;
    let dir_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("{} : nom illisible", path.display()))?;
    let name = dir_name
        .strip_suffix(".app")
        .filter(|name| !name.is_empty())
        .with_context(|| {
            format!("« {dir_name} » n'est pas un bundle d'app : son nom doit finir par .app")
        })?
        .to_owned();
    ensure!(path.is_dir(), "{} n'est pas un dossier", path.display());

    let info_path = path.join("Contents/Info.plist");
    let value = plist::Value::from_file(&info_path)
        .with_context(|| format!("lecture de {}", info_path.display()))?;
    let info = BundleInfo::from_plist(&value, &info_path.display().to_string())?;
    Ok(Bundle { path, name, info })
}

/// Fichiers à produire.
pub(crate) struct Outputs {
    pub(crate) archive: PathBuf,
    pub(crate) dmg: Option<PathBuf>,
}

impl Outputs {
    /// Sans `force`, refuse tout de suite si l'un d'eux existe déjà.
    pub(crate) fn new(bundle: &Bundle, dir: &Path, dmg: bool, force: bool) -> Result<Self> {
        let stem = format!("{}-{}", bundle.name, bundle.info.version);
        let outputs = Self {
            archive: dir.join(format!("{stem}.app.tar.gz")),
            dmg: dmg.then(|| dir.join(format!("{stem}.dmg"))),
        };
        if !force {
            for path in std::iter::once(&outputs.archive).chain(&outputs.dmg) {
                ensure!(
                    path.symlink_metadata().is_err(),
                    "{} existe déjà : supprime-le, ou relance avec --force pour le remplacer",
                    path.display()
                );
            }
        }
        Ok(outputs)
    }
}

/// Signature de code du bundle, d'après `codesign -dv`.
#[derive(Debug, PartialEq, Eq)]
enum CodeSignature {
    Unsigned,
    AdHoc,
    /// Signé avec un certificat, dont `authority` est le nom.
    Certificate {
        authority: String,
    },
}

/// Exige un bundle signé avec un certificat, et dont la signature est valide. Rend le nom du
/// certificat.
pub(crate) fn require_certificate(bundle: &Path) -> Result<String> {
    let output = run_tool(
        Command::new("codesign")
            .arg("-dv")
            .arg("--verbose=2")
            .arg(bundle),
    )?;
    let details = String::from_utf8_lossy(&output.stderr);
    let authority = match parse_codesign_details(output.status.success(), &details)? {
        CodeSignature::Certificate { authority } => authority,
        CodeSignature::AdHoc => bail!(
            "{} est signé ad hoc : cette signature ne vaut que pour ce binaire exact, et aucune mise à jour ne passerait le contrôle de signature de code. Signe le bundle avec ton certificat (codesign --sign \"<nom du certificat>\").",
            bundle.display()
        ),
        CodeSignature::Unsigned => bail!(
            "{} n'est pas signé : signe-le avec ton certificat (codesign --sign \"<nom du certificat>\")",
            bundle.display()
        ),
    };
    verify_code_signature(bundle)?;
    Ok(authority)
}

/// Lit la sortie de `codesign -dv --verbose=2`, que codesign écrit sur sa sortie d'erreur.
fn parse_codesign_details(success: bool, details: &str) -> Result<CodeSignature> {
    if details.contains("code object is not signed at all") {
        return Ok(CodeSignature::Unsigned);
    }
    ensure!(success, "codesign -dv a échoué : {}", details.trim());
    let adhoc = details.lines().any(|line| {
        line == "Signature=adhoc" || (line.starts_with("CodeDirectory ") && line.contains("adhoc"))
    });
    if adhoc {
        return Ok(CodeSignature::AdHoc);
    }
    details
        .lines()
        .find_map(|line| line.strip_prefix("Authority="))
        .map(|authority| CodeSignature::Certificate {
            authority: authority.to_owned(),
        })
        .with_context(|| format!("signature de code sans certificat : {}", details.trim()))
}

/// `codesign --verify --strict --deep` : le bundle n'a pas changé depuis sa signature.
fn verify_code_signature(bundle: &Path) -> Result<()> {
    let output = run_tool(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep"])
            .arg(bundle),
    )?;
    ensure!(
        output.status.success(),
        "signature de code invalide pour {} : {}",
        bundle.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Écrit l'archive, la contrôle, puis crée le DMG si demandé. Rend le chemin de l'ancienne
/// signature de l'archive, si elle a dû être supprimée.
pub(crate) fn package(bundle: &Bundle, outputs: &Outputs, force: bool) -> Result<Option<PathBuf>> {
    let top = bundle.dir_name();
    let work = files::work_dir_beside(&outputs.archive)?;
    let archive = work.path().join("archive.app.tar.gz");
    let file = File::create(&archive).context("création de l'archive")?;
    write_archive(&bundle.path, &top, BufWriter::new(&file)).context("écriture de l'archive")?;
    file.sync_all()?;

    let staging = tempfile::tempdir().context("création d'un dossier temporaire")?;
    check_archive(&archive, &top, staging.path())?;
    files::move_into_place(&archive, &outputs.archive, force)?;

    // Une signature laissée à côté ne correspond plus à la nouvelle archive.
    let stale = files::with_suffix(&outputs.archive, ".sig");
    let removed = match fs::remove_file(&stale) {
        Ok(()) => Some(stale),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("suppression de {}", stale.display()));
        }
    };

    if let Some(dmg) = &outputs.dmg {
        // Le DMG reprend le bundle extrait de l'archive : exactement ce que reçoit une mise à jour.
        let work = files::work_dir_beside(dmg)?;
        let image = work.path().join("image.dmg");
        create_dmg(staging.path(), &bundle.name, &image)?;
        files::move_into_place(&image, dmg, force)?;
    }
    Ok(removed)
}

/// Écrit l'archive gzip de `bundle`, sous le seul dossier `top`.
fn write_archive(bundle: &Path, top: &str, out: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(GzEncoder::new(out, Compression::default()));
    append_tree(&mut builder, bundle, Path::new(top))?;
    builder.into_inner()?.finish()?.flush()?;
    Ok(())
}

/// Ajoute `source` et son contenu sous le nom `name`, sans suivre les liens symboliques.
fn append_tree<W: Write>(builder: &mut tar::Builder<W>, source: &Path, name: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("lecture de {}", source.display()))?;
    let mut header = Header::new_gnu();
    // Les droits et la date sont gardés ; le propriétaire, propre à ce Mac, ne l'est pas.
    header.set_mode(metadata.mode() & 0o7777);
    header.set_mtime(u64::try_from(metadata.mtime()).unwrap_or(0));
    header.set_uid(0);
    header.set_gid(0);
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        header.set_entry_type(EntryType::Directory);
        header.set_size(0);
        builder.append_data(&mut header, name, io::empty())?;
        let mut entries = fs::read_dir(source)
            .with_context(|| format!("lecture de {}", source.display()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            let path = source.join(&entry);
            ensure!(
                !entry.as_bytes().starts_with(b"._"),
                "{} : fichier AppleDouble dans le bundle. Retire-le (dot_clean), puis signe de nouveau le bundle.",
                path.display()
            );
            append_tree(builder, &path, &name.join(&entry))?;
        }
    } else if file_type.is_symlink() {
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        let target = fs::read_link(source)?;
        builder.append_link(&mut header, name, target)?;
    } else if file_type.is_file() {
        let mut file =
            File::open(source).with_context(|| format!("lecture de {}", source.display()))?;
        let len = file.metadata()?.len();
        header.set_entry_type(EntryType::Regular);
        header.set_size(len);
        let mut contents = (&mut file).take(len);
        builder.append_data(&mut header, name, &mut contents)?;
        ensure!(
            contents.limit() == 0,
            "{} a changé pendant l'empaquetage",
            source.display()
        );
    } else {
        bail!(
            "{} : type de fichier impossible dans un bundle (tube, socket ou périphérique)",
            source.display()
        );
    }
    Ok(())
}

/// Extrait l'archive dans `dir`, vide, avec `/usr/bin/tar` comme le font les apps, puis contrôle
/// qu'elle contient le seul dossier `top` et que sa signature de code est toujours valide.
fn check_archive(archive: &Path, top: &str, dir: &Path) -> Result<()> {
    let output = run_tool(
        Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dir),
    )?;
    ensure!(
        output.status.success(),
        "/usr/bin/tar n'extrait pas l'archive : {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let names = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    ensure!(
        names.len() == 1 && names[0] == top,
        "l'archive devrait contenir le seul dossier {top}, elle contient : {}",
        names
            .iter()
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ")
    );
    verify_code_signature(&dir.join(top)).context("bundle extrait de l'archive")
}

/// Crée le DMG `out` à partir de `staging`, qui contient le bundle, en y ajoutant un raccourci
/// vers /Applications.
fn create_dmg(staging: &Path, volume: &str, out: &Path) -> Result<()> {
    symlink("/Applications", staging.join("Applications"))
        .context("création du raccourci vers /Applications")?;
    let output = run_tool(
        Command::new("hdiutil")
            .args(["create", "-quiet", "-volname", volume])
            .args(["-fs", "HFS+", "-format", "UDZO", "-srcfolder"])
            .arg(staging)
            .arg(out),
    )?;
    ensure!(
        output.status.success(),
        "hdiutil n'a pas créé le DMG : {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn run_tool(command: &mut Command) -> Result<Output> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .output()
        .with_context(|| format!("lancement de {program}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use flate2::read::GzDecoder;

    /// Fausse app : un exécutable Mach-O copié de /usr/bin/true, des liens symboliques, un dossier
    /// vide, un chemin et une cible de lien de plus de 100 caractères (extensions GNU de tar).
    pub(crate) fn fake_app(dir: &Path, name: &str, version: &str) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        let resources = app.join("Contents/Resources");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(resources.join("A")).unwrap();
        fs::copy("/usr/bin/true", app.join("Contents/MacOS").join(name)).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>fr.example.{name}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        fs::write(resources.join("A/data.txt"), "données\n").unwrap();
        symlink("A", resources.join("Current")).unwrap();
        let long = format!("{}/{}", "d".repeat(60), "e".repeat(60));
        fs::create_dir_all(resources.join(&long)).unwrap();
        fs::write(resources.join(&long).join("file.txt"), "profond\n").unwrap();
        symlink(format!("{long}/file.txt"), resources.join("longlink")).unwrap();
        fs::create_dir(resources.join("vide")).unwrap();
        app
    }

    pub(crate) fn sign_adhoc(app: &Path) {
        let status = Command::new("codesign")
            .args(["--sign", "-", "--force"])
            .arg(app)
            .output()
            .unwrap()
            .status;
        assert!(status.success());
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        Dir(u32),
        File(u32, Vec<u8>),
        Link(PathBuf),
    }

    /// Arborescence sous `root` : chemin relatif → type, droits, contenu ou cible.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Node> {
        let mut nodes = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let mode = metadata.permissions().mode() & 0o777;
            let node = if metadata.file_type().is_symlink() {
                Node::Link(fs::read_link(&path).unwrap())
            } else if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                Node::Dir(mode)
            } else {
                Node::File(mode, fs::read(&path).unwrap())
            };
            nodes.insert(relative, node);
        }
        nodes
    }

    #[test]
    fn codesign_details_are_classified() {
        let adhoc = "Executable=/x/Fake.app/Contents/MacOS/Fake\nIdentifier=fr.example.Fake\nFormat=app bundle with Mach-O thin (arm64)\nCodeDirectory v=20400 size=296 flags=0x2(adhoc) hashes=3+3 location=embedded\nSignature=adhoc\nInfo.plist entries=5\nTeamIdentifier=not set\n";
        assert_eq!(
            parse_codesign_details(true, adhoc).unwrap(),
            CodeSignature::AdHoc
        );
        let linker_signed = "CodeDirectory v=20400 size=381 flags=0x20002(adhoc,linker-signed) hashes=9+0 location=embedded\nSignature=adhoc\nSealed Resources=none\n";
        assert_eq!(
            parse_codesign_details(true, linker_signed).unwrap(),
            CodeSignature::AdHoc
        );
        assert_eq!(
            parse_codesign_details(false, "Bare.app: code object is not signed at all\n").unwrap(),
            CodeSignature::Unsigned
        );
        let certificate = "Identifier=fr.example.App\nCodeDirectory v=20500 size=12355 flags=0x10000(runtime) hashes=375+7 location=embedded\nSignature size=1700\nAuthority=Camille Essai\nSigned Time=24 Oct 2026 at 10:00:00\nTeamIdentifier=not set\n";
        assert_eq!(
            parse_codesign_details(true, certificate).unwrap(),
            CodeSignature::Certificate {
                authority: "Camille Essai".to_owned()
            }
        );
        assert!(parse_codesign_details(false, "x.app: No such file or directory\n").is_err());
    }

    #[test]
    fn bundle_version_is_read_and_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = read_bundle(&fake_app(dir.path(), "Demo", "1.2")).unwrap();
        assert_eq!(bundle.name, "Demo");
        assert_eq!(bundle.info.version.to_string(), "1.2.0");
        assert_eq!(bundle.info.raw_version, "1.2");
        assert_eq!(bundle.info.identifier.as_deref(), Some("fr.example.Demo"));
        assert_eq!(bundle.info.minimum_macos.as_deref(), Some("14.0"));
        assert_eq!(bundle.info.executable.as_deref(), Some("Demo"));

        let error = read_bundle(&fake_app(dir.path(), "Build", "1.2.3+4")).unwrap_err();
        assert!(
            format!("{error:#}").contains("métadonnées de build"),
            "{error:#}"
        );
        let folder = dir.path().join("Dossier");
        fs::create_dir(&folder).unwrap();
        assert!(read_bundle(&folder).is_err());
        assert!(read_bundle(&dir.path().join("Absent.app")).is_err());
    }

    #[test]
    fn archive_has_one_top_folder_and_keeps_links_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "1.0.0");
        let archive = dir.path().join("Demo.app.tar.gz");
        write_archive(&app, "Demo.app", File::create(&archive).unwrap()).unwrap();

        // Extraction par le tar du système, comme dans les apps.
        let out = dir.path().join("extrait");
        fs::create_dir(&out).unwrap();
        let status = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        let top: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(top, ["Demo.app"]);
        assert_eq!(tree(&out.join("Demo.app")), tree(&app));
        let executable = fs::metadata(out.join("Demo.app/Contents/MacOS/Demo")).unwrap();
        assert_eq!(executable.permissions().mode() & 0o111, 0o111);

        // Les en-têtes eux-mêmes : droits d'origine, propriétaire neutre, aucun fichier ._*.
        let mut entries = tar::Archive::new(GzDecoder::new(File::open(&archive).unwrap()));
        for entry in entries.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().into_owned();
            assert!(path.starts_with("Demo.app"), "{}", path.display());
            assert!(
                path.iter()
                    .all(|part| !part.as_encoded_bytes().starts_with(b"._")),
                "{}",
                path.display()
            );
            assert_eq!(entry.header().uid().unwrap(), 0);
            let source = fs::symlink_metadata(dir.path().join(&path)).unwrap();
            if !source.file_type().is_symlink() {
                assert_eq!(
                    entry.header().mode().unwrap(),
                    source.permissions().mode() & 0o7777,
                    "{}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn apple_double_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "1.0.0");
        fs::write(app.join("Contents/Resources/._data.txt"), "détritus").unwrap();
        let error = write_archive(&app, "Demo.app", io::sink()).unwrap_err();
        assert!(error.to_string().contains("AppleDouble"), "{error}");
    }

    /// Détache l'image à la fin du test, même en cas d'échec.
    struct Mounted(PathBuf);

    impl Drop for Mounted {
        fn drop(&mut self) {
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&self.0)
                .status();
        }
    }

    #[test]
    fn package_checks_the_archive_then_builds_the_dmg() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "2.1");
        // L'outil refuse l'ad hoc avant d'arriver ici ; il suffit pour contrôler l'archive.
        sign_adhoc(&app);
        let bundle = read_bundle(&app).unwrap();
        let out = dir.path().join("dist");
        fs::create_dir(&out).unwrap();
        let outputs = Outputs::new(&bundle, &out, true, false).unwrap();
        assert_eq!(outputs.archive, out.join("Demo-2.1.0.app.tar.gz"));
        assert_eq!(outputs.dmg.as_deref(), Some(&*out.join("Demo-2.1.0.dmg")));
        assert_eq!(package(&bundle, &outputs, false).unwrap(), None);

        // Rien ne traîne dans le dossier de sortie.
        let mut produced: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        produced.sort();
        assert_eq!(produced, ["Demo-2.1.0.app.tar.gz", "Demo-2.1.0.dmg"]);

        let mountpoint = dir.path().join("volume");
        fs::create_dir(&mountpoint).unwrap();
        let attached = Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-readonly", "-noautoopen"])
            .arg("-mountpoint")
            .arg(&mountpoint)
            .arg(outputs.dmg.as_ref().unwrap())
            .status()
            .unwrap();
        assert!(attached.success());
        let mounted = Mounted(mountpoint);
        let mut names: Vec<_> = fs::read_dir(&mounted.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| !name.as_encoded_bytes().starts_with(b"."))
            .collect();
        names.sort();
        assert_eq!(names, ["Applications", "Demo.app"]);
        assert_eq!(
            fs::read_link(mounted.0.join("Applications")).unwrap(),
            PathBuf::from("/Applications")
        );
        verify_code_signature(&mounted.0.join("Demo.app")).unwrap();
        drop(mounted);

        // Une seconde fois : refusé sans --force ; avec, remplacé, et l'ancienne signature retirée.
        assert!(Outputs::new(&bundle, &out, true, false).is_err());
        let stale = out.join("Demo-2.1.0.app.tar.gz.sig");
        fs::write(&stale, "ancienne").unwrap();
        let outputs = Outputs::new(&bundle, &out, false, true).unwrap();
        assert_eq!(
            package(&bundle, &outputs, true).unwrap(),
            Some(stale.clone())
        );
        assert!(!stale.exists());
    }

    #[test]
    fn a_bundle_modified_after_signing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let app = fake_app(dir.path(), "Demo", "1.0.0");
        sign_adhoc(&app);
        fs::write(app.join("Contents/Resources/A/data.txt"), "modifié").unwrap();
        let error = verify_code_signature(&app).unwrap_err();
        assert!(
            error.to_string().contains("signature de code invalide"),
            "{error}"
        );
    }
}
