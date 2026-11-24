//! Essais de `listing` par le binaire, contre le faux serveur de `tests/support/fake_server.rs`,
//! qui suit docs/API.md (« Captures d'écran »), et de l'avertissement de `doctor` sur la portée
//! `listing:write`. Les captures sont de vraies images PNG, construites en mémoire.

mod support;

use std::fs;
use std::io::Write;
use std::iter;
use std::path::{Path, PathBuf};
use std::process::Output;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use support::fake_server::{FakeServer, State};
use support::{both, drakbase, login, run, stderr, stdout};

/// Image PNG complète, en niveaux de gris, d'un seul ton : `shade` change son SHA-256.
fn png(width: u32, height: u32, shade: u8) -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend((data.len() as u32).to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        png.extend(kind);
        png.extend(data);
        png.extend(crc.sum().to_be_bytes());
    }
    let mut header = Vec::new();
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    header.extend([8, 0, 0, 0, 0]);
    // Chaque ligne commence par son filtre (0, aucun).
    let row: Vec<u8> = iter::once(0)
        .chain(iter::repeat_n(shade, width as usize))
        .collect();
    let mut pixels = ZlibEncoder::new(Vec::new(), Compression::fast());
    for _ in 0..height {
        pixels.write_all(&row).unwrap();
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &pixels.finish().unwrap());
    chunk(&mut png, b"IEND", &[]);
    png
}

/// Écrit `name` dans le dossier de l'essai ; rend son chemin.
fn file(home: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let dir = home.join("captures");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

/// `--screenshot <fichier>[=<légende>]`.
fn shot(path: &Path, caption: Option<&str>) -> String {
    match caption {
        Some(caption) => format!("--screenshot={}={caption}", path.display()),
        None => format!("--screenshot={}", path.display()),
    }
}

/// `drakbase listing test-app`, puis `extra`.
fn listing(home: &Path, extra: &[String]) -> Output {
    run(drakbase(home).args(["listing", "test-app"]).args(extra))
}

/// Captures de `test-app` sur le faux serveur : `(sha256, légende)`, dans l'ordre.
fn screenshots(server: &FakeServer) -> Vec<(String, String)> {
    server.state().apps["test-app"]
        .screenshots
        .iter()
        .map(|screenshot| (screenshot.sha256.clone(), screenshot.caption.clone()))
        .collect()
}

/// Taille d'une image comme l'écrit l'outil : « 2,9 Kio ».
fn size(png: &[u8]) -> String {
    am_protocol::human_size(png.len() as u64)
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Les requêtes qui ont modifié la fiche : `(méthode, chemin)`.
fn changes(server: &FakeServer) -> Vec<(String, String)> {
    server
        .state()
        .listing_changes()
        .iter()
        .map(|request| (request.method.clone(), request.path.clone()))
        .collect()
}

fn assert_contains(text: &str, expected: &[&str]) {
    for expected in expected {
        assert!(
            text.contains(expected),
            "« {expected} » absent de :\n{text}"
        );
    }
}

#[test]
fn the_listing_is_shown_without_the_listing_scope() {
    let mut state = State::demo(None);
    let app = state.apps.get_mut("test-app").unwrap();
    app.description = "Traduit tes textes.\n\nSans compte, sans pisteur.".to_owned();
    let (a, b) = (png(1280, 800, 1), png(640, 480, 2));
    state.add_screenshot("test-app", &a, "Écran principal");
    state.add_screenshot("test-app", &b, "");
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    let output = login(home.path(), &server.url, "amk_essai_ecriture");
    assert!(output.status.success(), "{}", both(&output));

    let output = listing(home.path(), &[]);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            "App     : test-app (Test App)",
            "Description :\n  Traduit tes textes.\n\n  Sans compte, sans pisteur.\n",
            "Captures d'écran : 2 sur 10\n",
            &format!("  1. 1280 × 800, {}, « Écran principal »\n", size(&a)),
            &format!("  2. 640 × 480, {}, sans légende\n", size(&b)),
        ],
    );
    assert!(changes(&server).is_empty());

    // Sans rien sur la fiche.
    let server = FakeServer::start(State::demo(None));
    login(home.path(), &server.url, "amk_essai_ecriture");
    let output = listing(home.path(), &[]);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &["Description : aucune\n", "Captures d'écran : aucune\n"],
    );
}

#[test]
fn the_server_follows_the_command() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    let output = login(home.path(), &server.url, "amk_essai_complet");
    assert!(output.status.success(), "{}", both(&output));
    let (a, b, c) = (png(1280, 800, 1), png(640, 480, 2), png(400, 400, 3));
    let a_png = file(home.path(), "accueil.png", &a);
    let b_png = file(home.path(), "réglages.PNG", &b);
    let c_png = file(home.path(), "c=troisième.png", &c);
    let description = file(
        home.path(),
        "description.txt",
        b"\n  Traduit tes textes.\n\n",
    );

    // Description, deux captures dont une légendée : déjà dans l'ordre d'arrivée.
    let first = [
        "--description".to_owned(),
        description.display().to_string(),
        shot(&a_png, Some("Accueil")),
        shot(&b_png, None),
    ];
    let output = listing(home.path(), &first);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            "✓ Description remplacée (19 caractères).",
            &format!("✓ Capture envoyée : accueil.png (1280 × 800, {})", size(&a)),
            &format!("✓ Capture envoyée : réglages.PNG (640 × 480, {})", size(&b)),
            "✓ Légende de accueil.png : « Accueil »",
            "✓ Fiche de test-app à jour.",
            "Description :\n  Traduit tes textes.\n",
            &format!(
                "  1. 1280 × 800, {}, « Accueil »\n  2. 640 × 480, {}, sans légende\n",
                size(&a),
                size(&b)
            ),
        ],
    );
    assert!(!stdout(&output).contains("ordre"), "{}", stdout(&output));
    assert_eq!(
        server.state().apps["test-app"].description,
        "Traduit tes textes."
    );
    assert_eq!(
        screenshots(&server),
        [
            (sha256(&a), "Accueil".to_owned()),
            (sha256(&b), String::new())
        ]
    );
    let sent = changes(&server);
    let methods: Vec<&str> = sent.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(methods, ["PATCH", "POST", "POST", "PATCH"], "{sent:?}");
    {
        let state = server.state();
        let upload = state
            .requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap();
        assert_eq!(upload.header("content-type"), Some("image/png"));
        assert_eq!(
            upload.header("content-length"),
            Some(a.len().to_string().as_str())
        );
    }

    // La même commande : rien à faire, rien ne repart.
    let output = listing(home.path(), &first);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &["La fiche est déjà à jour : rien à modifier."],
    );
    assert_eq!(changes(&server).len(), 4);

    // Une nouvelle capture en tête, des légendes changées : seule la nouvelle part, puis l'ordre.
    let output = listing(
        home.path(),
        &[
            shot(&c_png, Some("Troisième")),
            shot(&b_png, Some("Réglages")),
            shot(&a_png, None),
        ],
    );
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            &format!(
                "✓ Capture envoyée : c=troisième.png (400 × 400, {})",
                size(&c)
            ),
            "✓ Légende de c=troisième.png : « Troisième »",
            "✓ Légende de réglages.PNG : « Réglages »",
            "✓ Légende de accueil.png : (vide)",
            "✓ Captures remises dans l'ordre demandé.",
        ],
    );
    assert_eq!(
        screenshots(&server),
        [
            (sha256(&c), "Troisième".to_owned()),
            (sha256(&b), "Réglages".to_owned()),
            (sha256(&a), String::new()),
        ]
    );
    let sent = changes(&server);
    let methods: Vec<&str> = sent[4..]
        .iter()
        .map(|(method, _)| method.as_str())
        .collect();
    assert_eq!(
        methods,
        ["POST", "PATCH", "PATCH", "PATCH", "PUT"],
        "{sent:?}"
    );
    assert!(sent[8].1.ends_with("/screenshots/order"), "{sent:?}");

    // Une seule capture : les deux autres sont retirées.
    let output = listing(home.path(), &[shot(&b_png, Some("Réglages"))]);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            &format!("✓ Capture retirée : 400 × 400, {}, « Troisième »", size(&c)),
            &format!("✓ Capture retirée : 1280 × 800, {}, sans légende", size(&a)),
            &format!(
                "Captures d'écran : 1 sur 10\n  1. 640 × 480, {}, « Réglages »\n",
                size(&b)
            ),
        ],
    );
    assert_eq!(screenshots(&server), [(sha256(&b), "Réglages".to_owned())]);
}

#[test]
fn removals_come_first_so_a_full_listing_can_change() {
    let mut state = State::demo(None);
    let kept = png(320, 320, 100);
    state.add_screenshot("test-app", &kept, "");
    for shade in 1..10 {
        state.add_screenshot("test-app", &png(320, 320, shade), "");
    }
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    login(home.path(), &server.url, "amk_essai_complet");
    let new = png(2048, 1536, 200);
    let new_png = file(home.path(), "nouvelle.png", &new);
    let kept_png = file(home.path(), "gardée.png", &kept);

    // Dix captures sur le serveur : envoyer d'abord serait refusé (409).
    let output = listing(
        home.path(),
        &[shot(&new_png, Some("Nouvelle")), shot(&kept_png, None)],
    );
    assert!(output.status.success(), "{}", both(&output));
    let sent = changes(&server);
    let methods: Vec<&str> = sent.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(&methods[..9], ["DELETE"; 9], "{sent:?}");
    assert_eq!(&methods[9..], ["POST", "PATCH", "PUT"], "{sent:?}");
    assert_eq!(
        screenshots(&server),
        [
            (sha256(&new), "Nouvelle".to_owned()),
            (sha256(&kept), String::new())
        ]
    );
}

#[test]
fn clear_screenshots_removes_them_all() {
    let mut state = State::demo(None);
    let first = png(1280, 800, 1);
    state.add_screenshot("test-app", &first, "Accueil");
    state.add_screenshot("test-app", &png(1280, 800, 2), "");
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    login(home.path(), &server.url, "amk_essai_complet");
    let a_png = file(home.path(), "a.png", &png(1280, 800, 1));

    // Avec une liste de captures, l'option n'a pas de sens : clap la refuse.
    let output = listing(
        home.path(),
        &["--clear-screenshots".to_owned(), shot(&a_png, None)],
    );
    assert_eq!(output.status.code(), Some(2), "{}", both(&output));
    assert!(changes(&server).is_empty());

    let output = listing(home.path(), &["--clear-screenshots".to_owned()]);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            &format!(
                "✓ Capture retirée : 1280 × 800, {}, « Accueil »",
                size(&first)
            ),
            "Captures d'écran : aucune\n",
        ],
    );
    assert!(screenshots(&server).is_empty());

    // Plus rien à retirer.
    let output = listing(home.path(), &["--clear-screenshots".to_owned()]);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(&stdout(&output), &["rien à modifier"]);
}

#[test]
fn a_dry_run_changes_nothing() {
    let mut state = State::demo(None);
    state.apps.get_mut("test-app").unwrap().description = "Ancienne.".to_owned();
    let old = png(1280, 800, 1);
    let kept = png(1280, 800, 2);
    state.add_screenshot("test-app", &old, "Ancienne");
    state.add_screenshot("test-app", &kept, "");
    let server = FakeServer::start(state);
    let home = tempfile::tempdir().unwrap();
    login(home.path(), &server.url, "amk_essai_complet");
    let new = png(800, 600, 3);
    let new_png = file(home.path(), "nouvelle.png", &new);
    let kept_png = file(home.path(), "gardée.png", &kept);
    let description = file(home.path(), "description.txt", b"Nouvelle.");
    let args = [
        "--dry-run".to_owned(),
        "--description".to_owned(),
        description.display().to_string(),
        shot(&new_png, Some("Écran")),
        shot(&kept_png, Some("Gardée")),
    ];

    let output = listing(home.path(), &args);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(
        &stdout(&output),
        &[
            "Changements prévus (--dry-run : rien n'est modifié) :",
            "  ~ description : remplacée (9 caractères)",
            &format!(
                "  - retirer la capture 1280 × 800, {}, « Ancienne »",
                size(&old)
            ),
            &format!("  + envoyer nouvelle.png (800 × 600, {})", size(&new)),
            "  ~ légende de nouvelle.png : (vide) → « Écran »",
            "  ~ légende de gardée.png : (vide) → « Gardée »",
            "  ↕ ordre : nouvelle.png, gardée.png",
        ],
    );
    assert!(changes(&server).is_empty());
    assert_eq!(server.state().apps["test-app"].description, "Ancienne.");
    assert_eq!(screenshots(&server).len(), 2);

    // Sans la portée : l'aperçu, et ce qu'il faudra pour l'appliquer.
    login(home.path(), &server.url, "amk_essai_ecriture");
    let output = listing(home.path(), &args);
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(&stdout(&output), &["  + envoyer nouvelle.png"]);
    assert_contains(
        &stderr(&output),
        &["attention : le jeton « Écriture seule » n'a pas la portée listing:write"],
    );
    assert!(changes(&server).is_empty());
}

#[test]
fn local_checks_come_before_any_request() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    login(home.path(), &server.url, "amk_essai_complet");
    let requests = server.state().requests.len();
    let good = file(home.path(), "bonne.png", &png(1280, 800, 1));
    let twin = file(home.path(), "jumelle.png", &png(1280, 800, 1));
    let text = file(home.path(), "texte.png", b"pas une image");
    let small = file(home.path(), "petite.png", &png(1280, 200, 1));
    let huge = file(home.path(), "enorme.png", &vec![0; 8 * 1024 * 1024 + 1]);
    let long = file(home.path(), "longue.txt", "a".repeat(4001).as_bytes());
    let eleven: Vec<String> = (0..11)
        .map(|shade| {
            let path = file(
                home.path(),
                &format!("{shade}.png"),
                &png(320, 320, shade as u8),
            );
            shot(&path, None)
        })
        .collect();
    let missing = home.path().join("absente.png");

    for (args, expected) in [
        (
            vec![shot(&text, None)],
            "texte.png : ce n'est pas une image PNG",
        ),
        (
            vec![shot(&small, None)],
            "petite.png fait 1280 × 200 pixels : de 320 à 8192 pixels de côté",
        ),
        (
            vec![shot(&huge, None)],
            "enorme.png fait 8,0 Mio : une capture fait 8 Mio au plus",
        ),
        (eleven, "11 captures demandées : 10 au plus par app"),
        (vec![shot(&good, Some(&"é".repeat(201)))], "légende de"),
        (
            vec![shot(&good, None), shot(&twin, Some("Bis"))],
            "sont la même image",
        ),
        (
            vec!["--description".to_owned(), long.display().to_string()],
            "trop longue : 4001 caractères, 4000 au plus",
        ),
        (vec![shot(&missing, None)], "absente.png : introuvable"),
        (
            vec!["--screenshot=capture.jpg=Accueil".to_owned()],
            "un fichier .png attendu",
        ),
    ] {
        let output = listing(home.path(), &args);
        assert!(!output.status.success(), "{expected}\n{}", both(&output));
        assert!(
            stderr(&output).contains(expected),
            "{expected}\n{}",
            both(&output)
        );
    }
    assert_eq!(server.state().requests.len(), requests, "aucune requête");
}

#[test]
fn a_token_without_the_scope_is_told_what_to_do() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    login(home.path(), &server.url, "amk_essai_ecriture");
    let description = file(home.path(), "description.txt", b"Nouvelle.");

    let output = listing(
        home.path(),
        &[
            "--description".to_owned(),
            description.display().to_string(),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert_contains(
        &stderr(&output),
        &[
            "le jeton « Écriture seule » n'a pas la portée listing:write",
            "crée un jeton avec la portée « listing:write » dans l'administration",
            "« drakbase login »",
        ],
    );
    assert!(changes(&server).is_empty());

    // Un jeton limité à une autre app.
    login(home.path(), &server.url, "amk_essai_autre_app");
    let output = listing(home.path(), &[]);
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    assert_contains(
        &stderr(&output),
        &["le jeton « Autre app » ne sert qu'à l'app autre, pas à test-app"],
    );
}

#[test]
fn doctor_warns_when_the_token_cannot_change_the_listing() {
    let server = FakeServer::start(State::demo(None));
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();
    let warning =
        "! le jeton ne peut pas modifier la fiche publique de l'app (portée listing:write absente)";

    login(home.path(), &server.url, "amk_essai_ecriture");
    let output = run(drakbase(home.path())
        .current_dir(empty.path())
        .arg("doctor"));
    assert_contains(&stdout(&output), &[warning]);

    login(home.path(), &server.url, "amk_essai_complet");
    let output = run(drakbase(home.path())
        .current_dir(empty.path())
        .arg("doctor"));
    assert!(!stdout(&output).contains(warning), "{}", stdout(&output));
}

/// L'aide donne la taille maximale d'une capture dans l'unité du serveur, binaire : 8 Mio.
#[test]
fn the_help_gives_the_size_limit_in_the_unit_of_the_server() {
    let home = tempfile::tempdir().unwrap();
    let output = run(drakbase(home.path()).args(["listing", "--help"]));
    assert!(output.status.success(), "{}", both(&output));
    assert_contains(&stdout(&output), &["8 Mio au plus chacune"]);
}
