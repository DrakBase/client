//! Adresse d'un serveur DrakBase : `https://hôte[:port]`, sans chemin. L'outil y publie, l'agent
//! y vérifie, et tous deux en tirent l'hôte des images d'une stack.
//!
//! La lecture est faite ici, sans dépendance : la forme permise est étroite, et tout ce qui en
//! sort est refusé.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Le serveur de production : l'adresse inscrite dans les apps, qui ne change plus.
pub const PRODUCTION_SERVER: &str = "https://updates.drakbase.com";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServerUrlError {
    #[error("adresse « {0} » illisible (https://hôte attendu)")]
    Unreadable(String),
    #[error("adresse « {0} » : http ou https attendu")]
    Scheme(String),
    #[error("adresse « {0} » : pas d'identifiant dans l'adresse")]
    Credentials(String),
    #[error("adresse « {0} » : le serveur se donne sans chemin (https://hôte)")]
    Path(String),
}

/// Adresse d'un serveur, réduite à son origine : schéma et hôte en minuscules, port par défaut
/// retiré, sans barre finale.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerUrl {
    /// `schéma://hôte[:port]`.
    base: String,
    /// Longueur du schéma et de `://` dans `base` : ce qui suit est l'hôte des images.
    host_start: usize,
    loopback: bool,
}

impl ServerUrl {
    /// Lit `http(s)://hôte[:port]`, avec une barre finale au plus. Sont refusés : un autre
    /// schéma, un identifiant (`user@`), un chemin, des paramètres, un fragment, et un hôte qui
    /// n'est ni un nom ASCII, ni une adresse IPv4, ni une adresse IPv6 entre crochets.
    pub fn parse(input: &str) -> Result<Self, ServerUrlError> {
        let trimmed = input.trim();
        let unreadable = || ServerUrlError::Unreadable(trimmed.to_owned());
        let (scheme, rest) = trimmed.split_once("://").ok_or_else(unreadable)?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "https" => 443,
            "http" => 80,
            _ => return Err(ServerUrlError::Scheme(trimmed.to_owned())),
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return Err(ServerUrlError::Credentials(trimmed.to_owned()));
        }
        if !matches!(tail, "" | "/") {
            return Err(ServerUrlError::Path(trimmed.to_owned()));
        }
        let (host, port) = split_port(authority).ok_or_else(unreadable)?;
        let (host, loopback) = read_host(host).ok_or_else(unreadable)?;
        let mut base = format!("{scheme}://{host}");
        if let Some(port) = port.filter(|port| *port != default_port) {
            base.push_str(&format!(":{port}"));
        }
        Ok(Self {
            base,
            host_start: scheme.len() + "://".len(),
            loopback,
        })
    }

    /// Le serveur de production.
    pub fn production() -> Self {
        Self::parse(PRODUCTION_SERVER).expect("adresse de production valide")
    }

    /// `schéma://hôte[:port]`, sans barre finale.
    pub fn as_str(&self) -> &str {
        &self.base
    }

    /// Hôte des images servies par ce serveur, avec son port s'il n'est pas celui du schéma :
    /// ce qui précède `/<app>/<image>` dans un compose publié (`updates.drakbase.com`).
    pub fn image_host(&self) -> &str {
        &self.base[self.host_start..]
    }

    /// Serveur sur cette machine : `localhost`, ou une adresse de bouclage.
    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    pub fn is_https(&self) -> bool {
        self.base.starts_with("https://")
    }

    /// Adresse d'une route du serveur : `path` commence par `/`.
    pub fn join(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

/// Sépare l'hôte et le port d'une autorité. Un `:` sans port, ou un port illisible, est refusé.
fn split_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let (host, after) = authority.split_at(end + 1);
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return None,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            Some(text.parse().ok()?)
        }
        Some(_) => return None,
        None => None,
    };
    Some((host, port))
}

/// L'hôte en forme canonique, et s'il désigne cette machine.
fn read_host(host: &str) -> Option<(String, bool)> {
    if let Some(inner) = host.strip_prefix('[') {
        let address = Ipv6Addr::from_str(inner.strip_suffix(']')?).ok()?;
        return Some((format!("[{address}]"), address.is_loopback()));
    }
    let host = host.to_ascii_lowercase();
    let label = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if host.len() > 253 || !host.split('.').all(label) {
        return None;
    }
    let loopback =
        host == "localhost" || Ipv4Addr::from_str(&host).is_ok_and(|address| address.is_loopback());
    Some((host, loopback))
}

impl fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)
    }
}

impl FromStr for ServerUrl {
    type Err = ServerUrlError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl Serialize for ServerUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.base)
    }
}

impl<'de> Deserialize<'de> for ServerUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_gives_the_image_host() {
        let server = ServerUrl::parse(" HTTPS://Maj.Example.fr/ ").unwrap();
        assert_eq!(server.as_str(), "https://maj.example.fr");
        assert_eq!(server.to_string(), "https://maj.example.fr");
        assert_eq!(server.image_host(), "maj.example.fr");
        assert_eq!(
            server.join("/api/v1/catalog"),
            "https://maj.example.fr/api/v1/catalog"
        );
        assert!(server.is_https() && !server.is_loopback());

        let local = ServerUrl::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(local.image_host(), "127.0.0.1:8080");
        assert!(local.is_loopback() && !local.is_https());
        assert!(ServerUrl::parse("http://localhost").unwrap().is_loopback());
        assert!(
            !ServerUrl::parse("http://192.168.1.4")
                .unwrap()
                .is_loopback()
        );

        // Le port par défaut du schéma n'apparaît pas dans le nom des images.
        let cases = [
            ("https://maj.example.fr:443", "maj.example.fr"),
            ("http://maj.example.fr:80", "maj.example.fr"),
            ("https://maj.example.fr:80", "maj.example.fr:80"),
            ("https://[::1]:8443", "[::1]:8443"),
            ("https://[0:0:0:0:0:0:0:1]", "[::1]"),
            (
                "http://host.docker.internal:8790",
                "host.docker.internal:8790",
            ),
        ];
        for (input, host) in cases {
            assert_eq!(
                ServerUrl::parse(input).unwrap().image_host(),
                host,
                "{input}"
            );
        }
        assert!(ServerUrl::parse("https://[::1]").unwrap().is_loopback());
    }

    #[test]
    fn refuses_what_is_not_a_server() {
        use ServerUrlError::*;
        let cases = [
            ("maj.example.fr", Unreadable("maj.example.fr".into())),
            ("https://", Unreadable("https://".into())),
            ("https://maj..fr", Unreadable("https://maj..fr".into())),
            ("https://maj_x.fr", Unreadable("https://maj_x.fr".into())),
            ("https://-maj.fr", Unreadable("https://-maj.fr".into())),
            (
                "https://maj.example.fr:",
                Unreadable("https://maj.example.fr:".into()),
            ),
            (
                "https://maj.example.fr:99999",
                Unreadable("https://maj.example.fr:99999".into()),
            ),
            ("https://[::1", Unreadable("https://[::1".into())),
            ("https://[::1]x", Unreadable("https://[::1]x".into())),
            ("https://été.fr", Unreadable("https://été.fr".into())),
            (
                "ftp://maj.example.fr",
                Scheme("ftp://maj.example.fr".into()),
            ),
            (
                "https://maj.example.fr/api",
                Path("https://maj.example.fr/api".into()),
            ),
            (
                "https://maj.example.fr/?x=1",
                Path("https://maj.example.fr/?x=1".into()),
            ),
            (
                "https://maj.example.fr#x",
                Path("https://maj.example.fr#x".into()),
            ),
            (
                "https://user:pass@maj.example.fr",
                Credentials("https://user:pass@maj.example.fr".into()),
            ),
        ];
        for (input, error) in cases {
            assert_eq!(ServerUrl::parse(input), Err(error), "{input}");
        }
    }

    #[test]
    fn production_and_serde() {
        let production = ServerUrl::production();
        assert_eq!(production.as_str(), PRODUCTION_SERVER);
        assert_eq!(production.image_host(), "updates.drakbase.com");
        let json = serde_json::to_string(&production).unwrap();
        assert_eq!(json, "\"https://updates.drakbase.com\"");
        assert_eq!(
            serde_json::from_str::<ServerUrl>(&json).unwrap(),
            production
        );
        assert!(serde_json::from_str::<ServerUrl>("\"updates.drakbase.com\"").is_err());
        assert_eq!(
            "https://maj.example.fr"
                .parse::<ServerUrl>()
                .unwrap()
                .as_str(),
            "https://maj.example.fr"
        );
    }
}
