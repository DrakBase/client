//! Versions : lecture souple à l'entrée, forme canonique SemVer à la sortie.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Version canonique : trois composants numériques, une pré-version éventuelle, jamais de
/// métadonnées de build. C'est la seule forme que le serveur annonce et que l'outil signe,
/// car le plugin Tauri lit les versions strictement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version(semver::Version);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("version vide")]
    Empty,
    #[error("version « {0} » : quatre composants ou plus")]
    TooManyComponents(String),
    #[error("version « {0} » illisible")]
    Invalid(String),
    #[error("version « {0} » : les métadonnées de build (+…) sont interdites")]
    BuildMetadata(String),
}

impl Version {
    /// Lit la version d'une publication. Accepte `v1.2` ou `1.2`, refuse `+build`.
    pub fn parse_release(input: &str) -> Result<Self, VersionError> {
        let version = parse_lenient(input)?;
        if !version.build.is_empty() {
            return Err(VersionError::BuildMetadata(input.trim().to_owned()));
        }
        Ok(Self(version))
    }

    /// Lit la version envoyée par un client. Mêmes règles, mais `+build` est ignoré.
    pub fn parse_client(input: &str) -> Result<Self, VersionError> {
        let mut version = parse_lenient(input)?;
        version.build = semver::BuildMetadata::EMPTY;
        Ok(Self(version))
    }

    pub fn major(&self) -> u64 {
        self.0.major
    }

    pub fn minor(&self) -> u64 {
        self.0.minor
    }

    pub fn patch(&self) -> u64 {
        self.0.patch
    }

    /// Pré-version, vide pour une version finale : `beta.2` pour `1.4.0-beta.2`.
    pub fn pre(&self) -> &str {
        self.0.pre.as_str()
    }

    pub fn is_prerelease(&self) -> bool {
        !self.0.pre.is_empty()
    }
}

fn parse_lenient(input: &str) -> Result<semver::Version, VersionError> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    if body.is_empty() {
        return Err(VersionError::Empty);
    }
    // Le cœur numérique s'arrête à la pré-version (-) ou aux métadonnées de build (+).
    let (core, rest) = body.split_at(body.find(['-', '+']).unwrap_or(body.len()));
    let padded = match core.split('.').count() {
        1 => format!("{core}.0.0{rest}"),
        2 => format!("{core}.0{rest}"),
        3 => body.to_owned(),
        _ => return Err(VersionError::TooManyComponents(trimmed.to_owned())),
    };
    semver::Version::parse(&padded).map_err(|_| VersionError::Invalid(trimmed.to_owned()))
}

/// Précédence SemVer 2.0.0 (§11). Les métadonnées de build étant toujours vides, cet ordre
/// reste cohérent avec l'égalité.
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp_precedence(&other.0)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Version {
    type Err = VersionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse_release(input)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_release(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_short_versions_and_strips_prefix() {
        assert_eq!(
            Version::parse_release(" v1.2 ").unwrap().to_string(),
            "1.2.0"
        );
        assert_eq!(Version::parse_release("3").unwrap().to_string(), "3.0.0");
        assert_eq!(
            Version::parse_release("1.2-beta.1").unwrap().to_string(),
            "1.2.0-beta.1"
        );
    }

    #[test]
    fn build_metadata_is_refused_for_releases_and_ignored_for_clients() {
        assert!(matches!(
            Version::parse_release("1.2.3+7"),
            Err(VersionError::BuildMetadata(_))
        ));
        assert_eq!(
            Version::parse_client("1.2.3+7").unwrap().to_string(),
            "1.2.3"
        );
    }

    #[test]
    fn prerelease_sorts_before_release() {
        let beta = Version::parse_release("1.4.0-beta.2").unwrap();
        let release = Version::parse_release("1.4.0").unwrap();
        assert!(beta < release);
        assert!(beta.is_prerelease());
        assert_eq!(beta.pre(), "beta.2");
    }

    #[test]
    fn serde_round_trip_uses_canonical_form() {
        let version: Version = serde_json::from_str("\"v2.1\"").unwrap();
        assert_eq!(serde_json::to_string(&version).unwrap(), "\"2.1.0\"");
    }
}
