//! La connexion de l'outil au serveur, la même pour `publish`, `release`, `listing`, `doctor`,
//! `init` et `login` : les identifiants rangés par `login`, le client de l'API, et ce que le
//! serveur dit du jeton (`GET /api/v1/publish/whoami`). C'est ici aussi qu'une erreur de l'API
//! reçoit son conseil, et qu'un jeton limité à une autre app est refusé.

use std::fmt;

use anyhow::{Result, anyhow, bail};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::api::{Api, ApiError, AppInfo, WhoAmI, WhoAmIToken};
use crate::credentials::{Credentials, Secrets, ServerUrl};
use crate::home::AppSlug;

/// Les portées d'un jeton : créer une version et y envoyer des artefacts, la publier, modifier la
/// fiche publique.
pub use am_protocol::publish::{
    SCOPE_LISTING_WRITE as LISTING_SCOPE, SCOPE_RELEASES_PUBLISH as PUBLISH_SCOPE,
    SCOPE_RELEASES_WRITE as WRITE_SCOPE,
};

/// Ce qu'il faut faire d'un jeton limité à une autre app.
pub const OTHER_APP_ADVICE: &str =
    "crée un jeton pour cette app, ou pour toutes tes apps, puis relance « drakbase login »";
/// En deçà, l'outil prévient que le jeton va expirer.
const EXPIRY_NOTICE: Duration = Duration::days(30);

/// Conseils par code d'erreur du contrat (docs/API.md).
pub type Advice = [(&'static str, &'static str)];

/// Les conseils qui valent pour toutes les commandes.
const COMMON_ADVICE: [(&str, &str); 4] = [
    (
        "unauthorized",
        "Le jeton est refusé : il a expiré ou a été révoqué. Crée-en un autre dans l'administration, puis relance « drakbase login ».",
    ),
    (
        "forbidden",
        "Le jeton n'a pas les droits nécessaires : sa portée, son app, ou le rôle de son propriétaire.",
    ),
    (
        "payload_too_large",
        "Le fichier dépasse la taille maximale du serveur (AM_MAX_ARTIFACT_BYTES, 1 Gio par défaut).",
    ),
    (
        "rate_limited",
        "Trop de requêtes : réessaie dans quelques minutes.",
    ),
];

/// Une erreur de l'API, précédée de ce qui était fait, avec un conseil quand son code en appelle
/// un.
pub fn failure(error: ApiError, action: impl fmt::Display) -> anyhow::Error {
    failure_with(error, action, &[])
}

/// [`failure`], avec les conseils propres à une commande : pour un même code, ils passent avant
/// les conseils communs.
pub fn failure_with(error: ApiError, action: impl fmt::Display, own: &Advice) -> anyhow::Error {
    let advice = own
        .iter()
        .chain(&COMMON_ADVICE)
        .find(|(code, _)| error.code() == Some(*code))
        .map(|(_, advice)| advice);
    match advice {
        Some(advice) => anyhow!("{action} : {error}\n{advice}"),
        None => anyhow!("{action} : {error}"),
    }
}

/// Ce qui empêche d'ouvrir la connexion. `doctor` en fait un compte rendu, les autres commandes
/// une erreur ([`ConnectError::into_error`]).
pub enum ConnectError {
    /// `login` n'a rien rangé : où il aurait dû le faire.
    NotLoggedIn { place: String },
    /// Ce qui est rangé est illisible, ou le client de l'API n'a pas pu être créé.
    Unreadable(anyhow::Error),
    /// Le serveur refuse le jeton, ou ne répond pas.
    Refused { server: ServerUrl, error: ApiError },
}

impl ConnectError {
    /// L'erreur d'une commande qui ne fait rien sans connexion.
    pub fn into_error(self) -> anyhow::Error {
        match self {
            ConnectError::NotLoggedIn { place } => anyhow!(
                "aucune connexion enregistrée dans {place} : connecte-toi d'abord avec « drakbase login »"
            ),
            ConnectError::Unreadable(error) => error,
            ConnectError::Refused { server, error } => {
                failure(error, format!("vérification du jeton auprès de {server}"))
            }
        }
    }
}

/// Une connexion ouverte : le jeton est connu du serveur.
pub struct Connection {
    pub api: Api,
    pub whoami: WhoAmI,
}

impl Connection {
    /// Ouvre la connexion enregistrée par `login`.
    pub fn open(secrets: &Secrets) -> Result<Self, ConnectError> {
        let credentials = secrets
            .login()
            .map_err(ConnectError::Unreadable)?
            .ok_or_else(|| ConnectError::NotLoggedIn {
                place: secrets.login_place(),
            })?;
        Self::connect(credentials)
    }

    /// Vérifie ces identifiants auprès du serveur.
    pub fn connect(credentials: Credentials) -> Result<Self, ConnectError> {
        let api =
            Api::new(credentials.server, credentials.token).map_err(ConnectError::Unreadable)?;
        match api.whoami() {
            Ok(whoami) => Ok(Self { api, whoami }),
            Err(error) => Err(ConnectError::Refused {
                server: api.server().clone(),
                error,
            }),
        }
    }

    pub fn server(&self) -> &ServerUrl {
        self.api.server()
    }

    /// Le nom du compte du jeton, ou son adresse s'il n'a pas de nom.
    pub fn account(&self) -> &str {
        let user = &self.whoami.user;
        if user.name.is_empty() {
            &user.email
        } else {
            &user.name
        }
    }

    /// « https://maj.example.fr, compte Camille, jeton « Mac » ».
    pub fn describe(&self) -> String {
        format!(
            "{}, compte {}, jeton « {} »",
            self.server(),
            self.account(),
            self.whoami.token.name
        )
    }

    /// Ce qu'il faut dire d'un jeton limité à une autre app que celle-ci ; `None` s'il sert à
    /// cette app.
    pub fn other_app(&self, app: &AppSlug) -> Option<String> {
        let token = &self.whoami.token;
        let only = token.app.as_ref()?;
        (*only != app.to_string()).then(|| {
            format!(
                "le jeton « {} » ne sert qu'à l'app {only}, pas à {app}",
                token.name
            )
        })
    }

    /// Refuse un jeton limité à une autre app : il ne la verrait même pas.
    pub fn require_app(&self, app: &AppSlug) -> Result<()> {
        match self.other_app(app) {
            Some(problem) => bail!("{problem} : {OTHER_APP_ADVICE}"),
            None => Ok(()),
        }
    }

    /// L'app, telle que la décrit le serveur.
    pub fn app(&self, app: &AppSlug) -> Result<AppInfo> {
        self.api
            .app(app)
            .map_err(|error| failure(error, format!("lecture de l'app {app}")))
    }
}

/// « le jeton « … » expire le 2026-11-02 » quand il expire bientôt, ou a déjà expiré.
pub fn expiry_warning(token: &WhoAmIToken) -> Option<String> {
    let text = token.expires_at.as_str();
    let expires = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    let now = OffsetDateTime::now_utc();
    if expires <= now {
        Some(format!(
            "le jeton « {} » a expiré le {}",
            token.name,
            day(text)
        ))
    } else if expires - now < EXPIRY_NOTICE {
        Some(format!(
            "le jeton « {} » expire le {} : prépare-en un autre dans l'administration",
            token.name,
            day(text)
        ))
    } else {
        None
    }
}

/// La date d'une date RFC 3339 : `2027-10-25` pour `2027-10-25T12:00:00Z`.
pub fn day(rfc3339: &str) -> &str {
    rfc3339.split_once('T').map_or(rfc3339, |(day, _)| day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(expires_at: OffsetDateTime) -> WhoAmIToken {
        WhoAmIToken {
            name: "Mac".to_owned(),
            scopes: vec![WRITE_SCOPE.to_owned()],
            app: None,
            expires_at: expires_at.format(&Rfc3339).unwrap(),
        }
    }

    #[test]
    fn expiry_is_announced_a_month_ahead() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(expiry_warning(&token(now + Duration::days(365))), None);
        let soon = expiry_warning(&token(now + Duration::days(3))).unwrap();
        assert!(soon.contains("expire le"), "{soon}");
        let past = expiry_warning(&token(now - Duration::days(1))).unwrap();
        assert!(past.contains("a expiré"), "{past}");
        assert_eq!(day("2027-10-25T12:00:00Z"), "2027-10-25");
    }

    #[test]
    fn the_advice_of_a_command_comes_before_the_common_one() {
        let refused = |code: &str| ApiError::Server {
            code: code.to_owned(),
            message: "refus".to_owned(),
        };
        let own: &Advice = &[("forbidden", "Conseil de la commande.")];
        assert_eq!(
            failure_with(refused("forbidden"), "envoi", own).to_string(),
            "envoi : refus (forbidden)\nConseil de la commande."
        );
        // Un code que la commande ne connaît pas garde le conseil commun.
        let common = failure_with(refused("rate_limited"), "envoi", own).to_string();
        assert!(common.ends_with("\nTrop de requêtes : réessaie dans quelques minutes."));
        let plain = failure(refused("forbidden"), "envoi").to_string();
        assert!(plain.contains("sa portée, son app, ou le rôle"), "{plain}");
        // Sans conseil connu, l'erreur seule.
        assert_eq!(
            failure(refused("conflict"), "envoi").to_string(),
            "envoi : refus (conflict)"
        );
    }
}
