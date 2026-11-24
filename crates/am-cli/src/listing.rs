//! `drakbase listing <app> [--description <fichier>] [--screenshot <fichier.png>[=<légende>]]…
//! [--clear-screenshots] [--dry-run]` : montre la fiche publique d'une app, sa description et ses
//! captures d'écran, ou la modifie (docs/API.md, « Captures d'écran » et « Publication par
//! l'outil »).
//!
//! Avec des `--screenshot`, les captures du serveur deviennent exactement cette liste, dans cet
//! ordre. Elles sont comparées par SHA-256 : une image déjà sur le serveur ne repart pas, et la
//! même commande relancée après un échec reprend là où elle s'était arrêtée. Les captures qui ne
//! sont plus voulues sont retirées d'abord, pour ne jamais dépasser 10 en route.
//!
//! Tout ce qui peut l'être est contrôlé avant la connexion : fichiers lisibles, PNG, dimensions,
//! taille, nombre, longueur des légendes et de la description. Modifier la fiche demande un jeton
//! avec la portée `listing:write` ; la montrer, non.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// Les limites de la fiche sont celles du contrat, que le serveur impose.
use am_protocol::listing::{
    MAX_CAPTION_CHARS, MAX_DESCRIPTION_CHARS, MAX_SCREENSHOT_BYTES, MAX_SCREENSHOTS,
    SCREENSHOT_SIDES, png_dimensions,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};

use crate::api::{ApiError, AppChanges, AppInfo, Screenshot, WhoAmIToken};
use crate::connection::{self, Advice, ConnectError, Connection, LISTING_SCOPE};
use crate::credentials::Secrets;
use crate::home::{AppSlug, HomeArgs};

#[derive(clap::Args)]
pub struct Args {
    /// App de la fiche (slug), par exemple translate
    #[arg(value_name = "APP")]
    app: AppSlug,

    /// Remplace la description par le contenu de ce fichier, en texte brut (UTF-8, paragraphes
    /// séparés par une ligne vide)
    #[arg(long, value_name = "FICHIER")]
    description: Option<PathBuf>,

    /// Capture d'écran en PNG, sa légende après « = » ; répétée, les captures de la fiche
    /// deviennent exactement cette liste, dans cet ordre
    #[arg(
        long = "screenshot",
        value_name = "FICHIER.png[=LÉGENDE]",
        value_parser = ScreenshotArg::parse
    )]
    screenshots: Vec<ScreenshotArg>,

    /// Retire toutes les captures d'écran de la fiche
    #[arg(long, conflicts_with = "screenshots")]
    clear_screenshots: bool,

    /// Montre ce qui changerait, sans rien modifier
    #[arg(long)]
    dry_run: bool,

    #[command(flatten)]
    home: HomeArgs,
}

/// Un `--screenshot` : le fichier, et la légende qui suit le premier `=` placé après `.png`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScreenshotArg {
    path: PathBuf,
    caption: String,
}

impl ScreenshotArg {
    /// `a=b.png=Écran principal` désigne le fichier `a=b.png`, avec la légende « Écran
    /// principal ». `.png` est cherché sans tenir compte de la casse.
    fn parse(value: &str) -> Result<Self, String> {
        // Minuscules ASCII seulement : les positions restent celles de `value`.
        let lower = value.to_ascii_lowercase();
        let Some(extension) = lower.find(".png") else {
            return Err("un fichier .png attendu, suivi de « =légende » en option".to_owned());
        };
        let after = extension + ".png".len();
        let (path, caption) = match value[after..].find('=') {
            Some(equal) => (&value[..after + equal], &value[after + equal + 1..]),
            None => (value, ""),
        };
        Ok(Self {
            path: PathBuf::from(path),
            caption: caption.trim().to_owned(),
        })
    }
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = args.home.resolve()?;
    // Contrôles locaux, avant toute connexion.
    let description = args
        .description
        .as_deref()
        .map(read_description)
        .transpose()?;
    let wanted = if args.clear_screenshots {
        Some(Vec::new())
    } else if args.screenshots.is_empty() {
        None
    } else {
        Some(load_screenshots(&args.screenshots)?)
    };

    let secrets = Secrets::select(&home)?;
    let session = Session::open(&secrets, &args.app)?;
    if description.is_none() && wanted.is_none() {
        println!();
        session.show(&session.app);
        return Ok(ExitCode::SUCCESS);
    }

    let plan = Plan::new(&session.app, description.as_deref(), wanted.as_deref());
    let token = &session.connection.whoami.token;
    let allowed = token.has_scope(LISTING_SCOPE);
    if !allowed && !args.dry_run {
        bail!("{}", missing_scope(token));
    }
    println!();
    if plan.is_empty() {
        println!("La fiche est déjà à jour : rien à modifier.");
        return Ok(ExitCode::SUCCESS);
    }
    if args.dry_run {
        println!("Changements prévus (--dry-run : rien n'est modifié) :");
        plan.describe();
        if !allowed {
            eprintln!("attention : {}", missing_scope(token));
        }
        return Ok(ExitCode::SUCCESS);
    }
    session.apply(&plan)?;
    let updated = session.connection.app(&session.slug)?;
    println!();
    println!("✓ Fiche de {} à jour.", session.slug);
    session.show(&updated);
    Ok(ExitCode::SUCCESS)
}

/// Lit la nouvelle description : texte brut, sans les blancs de début et de fin.
fn read_description(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData => {
            anyhow!(
                "description {} : le fichier n'est pas en UTF-8",
                path.display()
            )
        }
        _ => anyhow::Error::new(error).context(format!("description {}", path.display())),
    })?;
    let text = text.trim();
    let count = text.chars().count();
    ensure!(
        count <= MAX_DESCRIPTION_CHARS,
        "description {} trop longue : {count} caractères, {MAX_DESCRIPTION_CHARS} au plus",
        path.display()
    );
    Ok(text.to_owned())
}

/// Une capture à mettre sur la fiche, lue et contrôlée.
struct LocalScreenshot {
    path: PathBuf,
    caption: String,
    png: Vec<u8>,
    /// SHA-256 en hexadécimal minuscule, comme le donne le serveur.
    sha256: String,
    width: u32,
    height: u32,
}

impl LocalScreenshot {
    fn name(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }
}

/// Lit et contrôle les captures voulues, dans l'ordre : PNG, taille, dimensions, légende, aucune
/// image en double, 10 au plus.
fn load_screenshots(args: &[ScreenshotArg]) -> Result<Vec<LocalScreenshot>> {
    ensure!(
        args.len() <= MAX_SCREENSHOTS,
        "{} captures demandées : {MAX_SCREENSHOTS} au plus par app",
        args.len()
    );
    let mut loaded: Vec<LocalScreenshot> = Vec::new();
    for arg in args {
        let screenshot = load_screenshot(arg)?;
        if let Some(twin) = loaded
            .iter()
            .find(|other| other.sha256 == screenshot.sha256)
        {
            bail!(
                "{} et {} sont la même image : une capture n'apparaît qu'une fois",
                twin.path.display(),
                screenshot.path.display()
            );
        }
        loaded.push(screenshot);
    }
    Ok(loaded)
}

fn load_screenshot(arg: &ScreenshotArg) -> Result<LocalScreenshot> {
    let path = &arg.path;
    let count = arg.caption.chars().count();
    ensure!(
        count <= MAX_CAPTION_CHARS,
        "légende de {} trop longue : {count} caractères, {MAX_CAPTION_CHARS} au plus",
        path.display()
    );
    let size = fs::metadata(path)
        .with_context(|| format!("capture {}", path.display()))?
        .len();
    ensure!(
        size <= MAX_SCREENSHOT_BYTES,
        "{} fait {} : une capture fait 8 Mio au plus",
        path.display(),
        am_protocol::human_size(size)
    );
    let png = fs::read(path).with_context(|| format!("capture {}", path.display()))?;
    // Ce que contrôle le serveur, sans décoder l'image : sa signature et son en-tête.
    let (width, height) = png_dimensions(&png)
        .with_context(|| format!("{} : ce n'est pas une image PNG lisible", path.display()))?;
    ensure!(
        SCREENSHOT_SIDES.contains(&width) && SCREENSHOT_SIDES.contains(&height),
        "{} fait {width} × {height} pixels : de {} à {} pixels de côté",
        path.display(),
        SCREENSHOT_SIDES.start(),
        SCREENSHOT_SIDES.end()
    );
    Ok(LocalScreenshot {
        path: path.clone(),
        caption: arg.caption.clone(),
        sha256: hex::encode(Sha256::digest(&png)),
        png,
        width,
        height,
    })
}

/// Ce qui change sur la fiche.
struct Plan<'a> {
    /// La nouvelle description, si elle diffère.
    description: Option<&'a str>,
    /// Les captures voulues, si la commande en parle.
    wanted: Option<&'a [LocalScreenshot]>,
    /// Captures du serveur qui ne sont plus voulues.
    remove: Vec<&'a Screenshot>,
    /// Captures voulues absentes du serveur.
    upload: Vec<&'a LocalScreenshot>,
    /// Légendes à changer : la capture voulue, et la légende actuelle sur le serveur.
    captions: Vec<(&'a LocalScreenshot, &'a str)>,
    /// L'ordre obtenu après retraits et envois n'est pas celui voulu.
    reorder: bool,
}

impl<'a> Plan<'a> {
    fn new(
        app: &'a AppInfo,
        description: Option<&'a str>,
        wanted: Option<&'a [LocalScreenshot]>,
    ) -> Self {
        let description = description.filter(|text| *text != app.description);
        let mut plan = Self {
            description,
            wanted,
            remove: Vec::new(),
            upload: Vec::new(),
            captions: Vec::new(),
            reorder: false,
        };
        let Some(wanted) = wanted else {
            return plan;
        };
        let wanted_hashes: HashSet<&str> =
            wanted.iter().map(|local| local.sha256.as_str()).collect();
        plan.remove = app
            .screenshots
            .iter()
            .filter(|remote| !wanted_hashes.contains(remote.sha256.as_str()))
            .collect();
        for local in wanted {
            match app
                .screenshots
                .iter()
                .find(|remote| remote.sha256 == local.sha256)
            {
                None => {
                    plan.upload.push(local);
                    // Une capture arrive sans légende.
                    if !local.caption.is_empty() {
                        plan.captions.push((local, ""));
                    }
                }
                Some(remote) if remote.caption != local.caption => {
                    plan.captions.push((local, &remote.caption));
                }
                Some(_) => {}
            }
        }
        // Le serveur garde l'ordre des captures restantes et met les nouvelles à la fin.
        let after: Vec<&str> = app
            .screenshots
            .iter()
            .map(|remote| remote.sha256.as_str())
            .filter(|sha256| wanted_hashes.contains(sha256))
            .chain(plan.upload.iter().map(|local| local.sha256.as_str()))
            .collect();
        plan.reorder = after
            .iter()
            .copied()
            .ne(wanted.iter().map(|local| local.sha256.as_str()));
        plan
    }

    fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.remove.is_empty()
            && self.upload.is_empty()
            && self.captions.is_empty()
            && !self.reorder
    }

    /// Les changements prévus, un par ligne.
    fn describe(&self) {
        if let Some(description) = self.description {
            println!("  ~ description : {}", describe_text(description));
        }
        for remote in &self.remove {
            println!("  - retirer la capture {}", RemoteLabel(remote));
        }
        for local in &self.upload {
            println!(
                "  + envoyer {} ({} × {}, {})",
                local.name(),
                local.width,
                local.height,
                am_protocol::human_size(local.png.len() as u64)
            );
        }
        for (local, before) in &self.captions {
            println!(
                "  ~ légende de {} : {} → {}",
                local.name(),
                quoted(before),
                quoted(&local.caption)
            );
        }
        if self.reorder
            && let Some(wanted) = self.wanted
        {
            let names: Vec<String> = wanted.iter().map(LocalScreenshot::name).collect();
            println!("  ↕ ordre : {}", names.join(", "));
        }
    }
}

/// « « texte » », ou « (vide) ».
fn quoted(text: &str) -> String {
    if text.is_empty() {
        "(vide)".to_owned()
    } else {
        format!("« {text} »")
    }
}

/// Résumé d'une description : effacée, ou sa longueur.
fn describe_text(text: &str) -> String {
    if text.is_empty() {
        "effacée".to_owned()
    } else {
        format!("remplacée ({} caractères)", text.chars().count())
    }
}

/// Une capture du serveur : dimensions, taille et légende.
struct RemoteLabel<'a>(&'a Screenshot);

impl fmt::Display for RemoteLabel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let screenshot = self.0;
        write!(
            f,
            "{} × {}, {}, ",
            screenshot.width,
            screenshot.height,
            am_protocol::human_size(screenshot.size)
        )?;
        if screenshot.caption.is_empty() {
            f.write_str("sans légende")
        } else {
            write!(f, "« {} »", screenshot.caption)
        }
    }
}

/// Connexion au serveur, une fois le jeton et l'app contrôlés.
struct Session {
    connection: Connection,
    slug: AppSlug,
    app: AppInfo,
}

impl Session {
    fn open(secrets: &Secrets, slug: &AppSlug) -> Result<Self> {
        let connection = Connection::open(secrets).map_err(ConnectError::into_error)?;
        connection.require_app(slug)?;
        let app = connection.app(slug)?;
        println!("Serveur : {}", connection.describe());
        println!("App     : {} ({})", app.slug, app.name);
        if let Some(warning) = connection::expiry_warning(&connection.whoami.token) {
            eprintln!("attention : {warning}");
        }
        Ok(Self {
            connection,
            slug: slug.clone(),
            app,
        })
    }

    /// La fiche : sa description, puis ses captures dans l'ordre d'affichage.
    fn show(&self, app: &AppInfo) {
        if app.description.is_empty() {
            println!("Description : aucune");
        } else {
            println!("Description :");
            for line in app.description.lines() {
                if line.trim().is_empty() {
                    println!();
                } else {
                    println!("  {line}");
                }
            }
        }
        if app.screenshots.is_empty() {
            println!("Captures d'écran : aucune");
        } else {
            println!(
                "Captures d'écran : {} sur {MAX_SCREENSHOTS}",
                app.screenshots.len()
            );
            for (index, screenshot) in app.screenshots.iter().enumerate() {
                println!("  {}. {}", index + 1, RemoteLabel(screenshot));
            }
        }
    }

    /// Fait les changements, dans l'ordre : description, retraits, envois, légendes, ordre.
    fn apply(&self, plan: &Plan<'_>) -> Result<()> {
        let slug = &self.slug;
        if let Some(description) = plan.description {
            let changes = AppChanges {
                description: Some(description.to_owned()),
            };
            self.connection
                .api
                .update_app(slug, &changes)
                .map_err(|error| failure(error, "modification de la description"))?;
            println!("✓ Description {}.", describe_text(description));
        }
        for remote in &plan.remove {
            self.connection
                .api
                .delete_screenshot(slug, &remote.id)
                .map_err(|error| failure(error, "retrait d'une capture"))?;
            println!("✓ Capture retirée : {}", RemoteLabel(remote));
        }
        // Identifiant sur le serveur de chaque image, par SHA-256.
        let mut ids: Vec<(String, String, String)> = self
            .app
            .screenshots
            .iter()
            .map(|remote| {
                (
                    remote.sha256.clone(),
                    remote.id.clone(),
                    remote.caption.clone(),
                )
            })
            .collect();
        for local in &plan.upload {
            let (screenshot, created) = self
                .connection
                .api
                .add_screenshot(slug, &local.png)
                .map_err(|error| failure(error, format!("envoi de {}", local.path.display())))?;
            ensure!(
                screenshot.sha256.eq_ignore_ascii_case(&local.sha256),
                "envoi de {} : le serveur annonce un autre SHA-256 ({}) : l'image a changé en route, relance la commande",
                local.path.display(),
                screenshot.sha256
            );
            if created {
                println!(
                    "✓ Capture envoyée : {} ({} × {}, {})",
                    local.name(),
                    local.width,
                    local.height,
                    am_protocol::human_size(local.png.len() as u64)
                );
            } else {
                println!("✓ Capture déjà sur le serveur : {}", local.name());
            }
            ids.push((local.sha256.clone(), screenshot.id, screenshot.caption));
        }
        let id_of = |local: &LocalScreenshot| {
            ids.iter()
                .find(|(sha256, _, _)| *sha256 == local.sha256)
                .map(|(_, id, caption)| (id.as_str(), caption.as_str()))
        };
        if let Some(wanted) = plan.wanted {
            for local in wanted {
                let Some((id, caption)) = id_of(local) else {
                    continue;
                };
                if caption != local.caption {
                    self.connection
                        .api
                        .set_screenshot_caption(slug, id, &local.caption)
                        .map_err(|error| {
                            failure(error, format!("légende de {}", local.path.display()))
                        })?;
                    println!("✓ Légende de {} : {}", local.name(), quoted(&local.caption));
                }
            }
            if plan.reorder {
                let order: Vec<&str> = wanted
                    .iter()
                    .filter_map(|local| id_of(local).map(|(id, _)| id))
                    .collect();
                self.connection
                    .api
                    .order_screenshots(slug, &order)
                    .map_err(|error| failure(error, "ordre des captures"))?;
                println!("✓ Captures remises dans l'ordre demandé.");
            }
        }
        Ok(())
    }
}

/// Ce qu'il faut faire quand le jeton n'a pas la portée `listing:write`.
fn missing_scope(token: &WhoAmIToken) -> String {
    format!(
        "le jeton « {} » n'a pas la portée {LISTING_SCOPE}, nécessaire pour modifier la fiche publique : crée un jeton avec la portée « {LISTING_SCOPE} » dans l'administration (Mon compte, Jetons), puis relance « drakbase login »",
        token.name
    )
}

/// Les conseils propres à la fiche, qui passent avant ceux de toutes les commandes.
const LISTING_ADVICE: &Advice = &[
    (
        "forbidden",
        "Modifier la fiche demande un jeton avec la portée « listing:write », pour cette app : crée-le dans l'administration (Mon compte, Jetons), puis relance « drakbase login ».",
    ),
    (
        "payload_too_large",
        "Une capture fait 8 Mio au plus, et la limite de Caddy doit le permettre.",
    ),
];

/// Une erreur du serveur, avec ce qu'il faut faire ensuite.
fn failure(error: ApiError, action: impl fmt::Display) -> anyhow::Error {
    connection::failure_with(error, action, LISTING_ADVICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(path: &str, caption: &str) -> ScreenshotArg {
        ScreenshotArg {
            path: PathBuf::from(path),
            caption: caption.to_owned(),
        }
    }

    /// La limite d'une capture s'écrit « 8 Mio » dans les messages et dans l'aide : c'est celle
    /// du protocole, dans son unité.
    #[test]
    fn the_size_limit_of_a_screenshot_is_the_one_of_the_protocol() {
        assert_eq!(am_protocol::human_size(MAX_SCREENSHOT_BYTES), "8,0 Mio");
        let refused = ApiError::Server {
            code: "payload_too_large".to_owned(),
            message: "capture de 8 Mio au plus".to_owned(),
        };
        assert_eq!(
            failure(refused, "envoi de accueil.png").to_string(),
            "envoi de accueil.png : capture de 8 Mio au plus (payload_too_large)\nUne capture fait 8 Mio au plus, et la limite de Caddy doit le permettre."
        );
    }

    #[test]
    fn the_caption_follows_the_first_equal_sign_after_png() {
        assert_eq!(ScreenshotArg::parse("a.png").unwrap(), arg("a.png", ""));
        assert_eq!(
            ScreenshotArg::parse("a=b.png=Écran principal").unwrap(),
            arg("a=b.png", "Écran principal")
        );
        assert_eq!(
            ScreenshotArg::parse("Captures/Accueil.PNG= Vue d'ensemble = 1 ").unwrap(),
            arg("Captures/Accueil.PNG", "Vue d'ensemble = 1")
        );
        assert_eq!(
            ScreenshotArg::parse("x.png=voir y.png=z").unwrap(),
            arg("x.png", "voir y.png=z")
        );
        assert_eq!(ScreenshotArg::parse("x.png=").unwrap(), arg("x.png", ""));
        assert!(ScreenshotArg::parse("capture.jpg=Accueil").is_err());
    }

    fn remote(id: &str, sha256: &str, caption: &str) -> Screenshot {
        Screenshot {
            id: id.to_owned(),
            width: 1280,
            height: 800,
            size: 1000,
            sha256: sha256.to_owned(),
            caption: caption.to_owned(),
            url: format!("https://maj.example.fr/api/admin/apps/demo/screenshots/{id}"),
            created_at: "2026-10-29T08:00:00Z".to_owned(),
        }
    }

    fn local(sha256: &str, caption: &str) -> LocalScreenshot {
        LocalScreenshot {
            path: PathBuf::from(format!("{sha256}.png")),
            caption: caption.to_owned(),
            png: Vec::new(),
            sha256: sha256.to_owned(),
            width: 1280,
            height: 800,
        }
    }

    fn app(screenshots: Vec<Screenshot>) -> AppInfo {
        let mut app: AppInfo = serde_json::from_str(
            r#"{"slug": "demo", "name": "Démo", "kind": "swift", "description": "Avant",
                "screenshots": [], "keys": [], "latest": {}}"#,
        )
        .unwrap();
        app.screenshots = screenshots;
        app
    }

    #[test]
    fn the_plan_removes_first_then_sends_captions_and_orders() {
        let app = app(vec![
            remote("1", "a", ""),
            remote("2", "b", "B"),
            remote("3", "c", ""),
        ]);
        // c reste, a part, d arrive avec sa légende, b change de légende, l'ordre change.
        let wanted = vec![local("c", ""), local("d", "D"), local("b", "B2")];
        let plan = Plan::new(&app, Some("Avant"), Some(&wanted));
        assert!(plan.description.is_none(), "description inchangée");
        assert_eq!(
            plan.remove
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
        assert_eq!(
            plan.upload
                .iter()
                .map(|l| l.sha256.as_str())
                .collect::<Vec<_>>(),
            ["d"]
        );
        let captions: Vec<(&str, &str)> = plan
            .captions
            .iter()
            .map(|(local, before)| (local.sha256.as_str(), *before))
            .collect();
        assert_eq!(captions, [("d", ""), ("b", "B")]);
        // Après envoi : b, c, d ; voulu : c, d, b.
        assert!(plan.reorder);

        // Déjà dans l'ordre, nouvelles à la fin : rien à réordonner.
        let wanted = vec![local("a", ""), local("b", "B"), local("e", "")];
        let plan = Plan::new(&app, Some("Après"), Some(&wanted));
        assert_eq!(plan.description, Some("Après"));
        assert!(!plan.reorder);
        assert_eq!(plan.remove.len(), 1);
        assert!(plan.captions.is_empty());

        // Rien à faire.
        let wanted = vec![local("a", ""), local("b", "B"), local("c", "")];
        assert!(Plan::new(&app, None, Some(&wanted)).is_empty());
        // Tout retirer.
        let plan = Plan::new(&app, None, Some(&[]));
        assert_eq!(plan.remove.len(), 3);
        assert!(!plan.reorder);
    }

    #[test]
    fn a_refusal_says_which_scope_to_ask_for() {
        let refused = failure(
            ApiError::Server {
                code: "forbidden".to_owned(),
                message: "ce jeton n'a pas la portée listing:write".to_owned(),
            },
            "envoi de a.png",
        )
        .to_string();
        assert!(
            refused.starts_with(
                "envoi de a.png : ce jeton n'a pas la portée listing:write (forbidden)\n"
            ),
            "{refused}"
        );
        assert!(refused.contains("« listing:write »"), "{refused}");
        assert!(refused.contains("« drakbase login »"), "{refused}");
    }
}
