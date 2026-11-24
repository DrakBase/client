//! Version d'un OS, ou de Docker Compose pour une stack : des nombres séparés par des points,
//! comparés un à un (docs/PROTOCOLE.md, « Versions d'OS »). Le serveur s'en sert pour
//! `os_version` et `min_os_version`, l'agent pour Docker et pour `min_compose_version`.

use std::fmt;

/// Version comparée composant par composant ; un composant absent vaut 0, donc `14` égale
/// `14.0.0`.
///
/// Les composants sont gardés sans leurs zéros finaux : l'ordre des listes est alors l'ordre
/// numérique avec complément à 0.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsVersion(Vec<u64>);

impl OsVersion {
    /// Longueur maximale d'un texte lu, en octets.
    pub(crate) const MAX_LEN: usize = 64;

    /// Lecture souple, pour ce qu'envoie une machine : les composants numériques de tête
    /// comptent, le reste est ignoré (`6.8.0-45-generic` vaut 6.8.0, `2..1` vaut 2). `None` si le
    /// texte ne commence pas par un nombre (`v2.29`, `.5`), s'il dépasse 64 octets, ou si un
    /// nombre ne tient pas sur 64 bits.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.len() > Self::MAX_LEN {
            return None;
        }
        let mut components = Vec::new();
        for part in input.split('.') {
            let digits = part.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                break;
            }
            components.push(part[..digits].parse().ok()?);
            if digits < part.len() {
                break;
            }
        }
        Self::from_components(components)
    }

    /// Lecture stricte, pour ce qu'un développeur déclare (`min_compose_version`) : de 1 à 4
    /// nombres de 1 à 9 chiffres, séparés par des points, et rien d'autre.
    pub fn parse_exact(input: &str) -> Option<Self> {
        let parts: Vec<&str> = input.split('.').collect();
        let numeric = |part: &&str| {
            !part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit())
        };
        if !(1..=4).contains(&parts.len()) || !parts.iter().all(numeric) {
            return None;
        }
        Self::from_components(
            parts
                .iter()
                .map(|part| part.parse().ok())
                .collect::<Option<_>>()?,
        )
    }

    fn from_components(mut components: Vec<u64>) -> Option<Self> {
        while components.len() > 1 && components.last() == Some(&0) {
            components.pop();
        }
        (!components.is_empty()).then_some(Self(components))
    }

    /// Les composants, sans zéros finaux : `[14]` pour `14.0.0`, `[0]` pour `0.0`.
    pub fn components(&self) -> &[u64] {
        &self.0
    }

    /// Cette version vaut-elle au moins `minimum` ?
    pub fn at_least(&self, minimum: &Self) -> bool {
        self >= minimum
    }
}

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&text.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_reading_keeps_the_leading_numbers() {
        let parse = |raw| OsVersion::parse(raw);
        assert_eq!(parse("14.6").unwrap().components(), [14, 6]);
        assert_eq!(parse(" 10.0.22631 ").unwrap().components(), [10, 0, 22631]);
        assert_eq!(parse("6.8.0-45-generic"), parse("6.8"));
        assert_eq!(parse("14"), parse("14.0.0"));
        assert_eq!(parse("2..1"), parse("2"));
        assert_eq!(parse("0.0").unwrap().components(), [0]);
        for unreadable in ["", "abc", "v14", "v2.29", ".5", "-1"] {
            assert_eq!(parse(unreadable), None, "{unreadable}");
        }
        assert_eq!(OsVersion::parse(&"1.".repeat(40)), None);
        assert_eq!(parse("99999999999999999999999.1"), None);
        assert_eq!(parse("14.6.0").unwrap().to_string(), "14.6");
    }

    #[test]
    fn order_pads_with_zeros() {
        let ordered = ["9", "10", "10.0.1", "13.5", "14", "14.0.1", "14.6"];
        for pair in ordered.windows(2) {
            let (low, high) = (OsVersion::parse(pair[0]), OsVersion::parse(pair[1]));
            assert!(low < high, "{} < {}", pair[0], pair[1]);
        }
        let version = OsVersion::parse("2.29.1").unwrap();
        assert!(version.at_least(&OsVersion::parse("2.29").unwrap()));
        assert!(!version.at_least(&OsVersion::parse("2.30").unwrap()));
    }

    #[test]
    fn exact_reading_is_for_declared_versions() {
        assert_eq!(OsVersion::parse_exact("2.24"), OsVersion::parse("2.24.0"));
        assert_eq!(
            OsVersion::parse_exact("2.24.0.1").unwrap().components(),
            [2, 24, 0, 1]
        );
        for refused in [
            "",
            "v2.24",
            "2.24.0.1.2",
            "2..1",
            "2.24-rc1",
            " 2.24",
            "1234567890",
        ] {
            assert_eq!(OsVersion::parse_exact(refused), None, "{refused}");
        }
    }
}
