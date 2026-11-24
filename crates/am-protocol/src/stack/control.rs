//! Dossier de contrôle des mises à jour validées dans l'app (docs/PROTOCOLE.md, « Mises à jour
//! validées dans l'app ») : `status.json`, écrit par l'agent, et les demandes que l'app dépose
//! dans `request/`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::StackError;
use crate::platform::Channel;
use crate::version::Version;

pub const FORMAT: u64 = 1;
/// Taille maximale d'une demande.
pub const MAX_REQUEST_BYTES: usize = 4096;
/// Variable d'environnement qui donne à l'app le chemin du dossier de contrôle.
pub const ENV_CONTROL: &str = "DRAKBASE_CONTROL";
pub const STATUS_FILE: &str = "status.json";
pub const REQUEST_DIR: &str = "request";

/// `status.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub format: u64,
    pub app: String,
    /// Version installée et saine ; absente avant la première.
    pub installed: Option<Version>,
    pub channel: Channel,
    /// Dernière vérification réussie (RFC 3339 UTC).
    pub checked_at: Option<String>,
    pub available: Option<Available>,
    /// Version restée en échec (`rollback: manual`).
    pub failed: Option<Version>,
    pub operation: Option<Operation>,
}

/// Version plus récente, téléchargée et vérifiée par l'agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Available {
    pub version: Version,
    pub channel: Channel,
    pub pub_date: String,
    /// Markdown, éventuellement vide.
    pub notes: String,
    /// Libellés des permissions nouvelles : l'app ne propose alors pas d'installer.
    pub new_permissions: Vec<String>,
}

/// La dernière demande et son déroulement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub action: Action,
    /// Pour `update`.
    pub version: Option<Version>,
    pub state: OperationState,
    pub started_at: String,
    /// Absente pendant `running`.
    pub finished_at: Option<String>,
    /// Texte en français, à montrer tel quel.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Check,
    Update,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationState {
    Running,
    Done,
    Failed,
    /// La version demande des permissions nouvelles, à accepter sur la machine.
    Permissions,
}

impl Status {
    /// Texte JSON, avec un saut de ligne final.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        text
    }
}

/// Une demande de l'app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Check,
    Update { version: Version },
}

impl Request {
    /// Lit une demande : `{"format": 1, "action": "check"}` ou
    /// `{"format": 1, "action": "update", "version": "<version canonique>"}`, rien d'autre.
    pub fn parse(bytes: &[u8]) -> Result<Self, StackError> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(StackError::RequestTooLarge);
        }
        let object: Map<String, Value> = match serde_json::from_slice(bytes) {
            Ok(Value::Object(object)) => object,
            Ok(_) => {
                return Err(StackError::RequestInvalid(
                    "un objet JSON est attendu".into(),
                ));
            }
            Err(error) => return Err(StackError::RequestInvalid(error.to_string())),
        };
        if let Some(unknown) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "format" | "action" | "version"))
        {
            return Err(StackError::RequestInvalid(format!(
                "champ inconnu « {unknown} »"
            )));
        }
        match object.get("format") {
            Some(Value::Number(number)) if number.as_u64() == Some(FORMAT) => {}
            Some(Value::Number(number)) => {
                return Err(StackError::RequestFormat(number.to_string()));
            }
            Some(_) => return Err(StackError::RequestInvalid("format : nombre attendu".into())),
            None => return Err(StackError::RequestInvalid("format absent".into())),
        }
        let action = match object.get("action") {
            Some(Value::String(action)) => action.as_str(),
            Some(_) => return Err(StackError::RequestInvalid("action : texte attendu".into())),
            None => return Err(StackError::RequestInvalid("action absente".into())),
        };
        match (action, object.get("version")) {
            ("check", None) => Ok(Request::Check),
            ("check", Some(_)) => Err(StackError::RequestInvalid("check : pas de version".into())),
            ("update", Some(Value::String(raw))) => {
                let version = Version::parse_release(raw)
                    .ok()
                    .filter(|parsed| parsed.to_string() == *raw)
                    .ok_or_else(|| StackError::RequestVersion(raw.clone()))?;
                Ok(Request::Update { version })
            }
            ("update", Some(_)) => {
                Err(StackError::RequestInvalid("version : texte attendu".into()))
            }
            ("update", None) => Err(StackError::RequestInvalid(
                "update : version absente".into(),
            )),
            (other, _) => Err(StackError::RequestAction(other.to_owned())),
        }
    }
}
