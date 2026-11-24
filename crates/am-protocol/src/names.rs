//! Noms du contrat : le slug d'une app et le nom de fichier réduit d'un artefact.

/// Longueur maximale d'un nom de fichier réduit.
pub const MAX_FILENAME_CHARS: usize = 200;

/// Nom donné à un fichier dont le nom réduit ne serait fait que de points.
const FALLBACK_FILENAME: &str = "telechargement";

/// Slug d'une app : `[a-z0-9-]{1,64}`.
pub fn is_app_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// Nom de fichier réduit à `[A-Za-z0-9._-]{1,200}` (docs/API.md) : celui d'un artefact à son
/// envoi, puis celui de `Content-Disposition` à son téléchargement.
///
/// Tout autre caractère devient `_`, et le nom est coupé à `MAX_FILENAME_CHARS` caractères : la
/// fonction rend toujours un nom. Qui ne veut pas d'un nom coupé (l'outil, dont le nom entre dans
/// la signature) compare d'abord la longueur à `MAX_FILENAME_CHARS`.
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_FILENAME_CHARS)
        .collect();
    // Un nom vide, ou fait uniquement de points, serait trompeur (`.` ou `..`).
    if cleaned.chars().all(|c| c == '.') {
        FALLBACK_FILENAME.to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for good in ["home-manager", "a", "app2", "x".repeat(64).as_str()] {
            assert!(is_app_slug(good), "{good}");
        }
        for bad in [
            "",
            "Home",
            "a_b",
            "a.b",
            "é",
            "a b",
            "x".repeat(65).as_str(),
        ] {
            assert!(!is_app_slug(bad), "{bad}");
        }
    }

    #[test]
    fn filenames_are_reduced() {
        assert_eq!(
            sanitize_filename("Translate-1.2.0.app.tar.gz"),
            "Translate-1.2.0.app.tar.gz"
        );
        assert_eq!(sanitize_filename("Mon app (1).dmg"), "Mon_app__1_.dmg");
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename("été.exe"), "_t_.exe");
        // Chaque caractère en vaut un, quel que soit son nombre d'octets.
        assert_eq!(sanitize_filename(&"é".repeat(250)), "_".repeat(200));
        assert_eq!(
            sanitize_filename(&"a".repeat(201)).len(),
            MAX_FILENAME_CHARS
        );
        for dots in ["", ".", "..", "..."] {
            assert_eq!(sanitize_filename(dots), "telechargement", "{dots:?}");
        }
    }
}
