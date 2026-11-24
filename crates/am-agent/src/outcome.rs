//! Issue d'une commande et codes de sortie du contrat (docs/PROTOCOLE.md, « Agent
//! `drakbase-stack` ») : `0` à jour ou mis à jour, `1` échec, `2` usage, `3` permission à accepter.

use std::fmt;
use std::io;

/// Issue d'une commande qui n'a pas levé d'erreur. Un échec déjà expliqué (mise à jour qui ne
/// devient pas saine, par exemple) est un `Failed`, sans message de plus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    PermissionRequired,
    Failed,
}

impl Outcome {
    pub const USAGE: u8 = 2;

    pub const fn code(self) -> u8 {
        match self {
            Outcome::Done => 0,
            Outcome::Failed => 1,
            Outcome::PermissionRequired => 3,
        }
    }

    /// Issue de plusieurs stacks : un échec l'emporte sur une permission à accepter.
    pub fn worst(self, other: Outcome) -> Outcome {
        fn rank(outcome: Outcome) -> u8 {
            match outcome {
                Outcome::Done => 0,
                Outcome::PermissionRequired => 1,
                Outcome::Failed => 2,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// Erreur d'usage (code 2) : une demande qui ne peut pas être servie telle qu'elle est écrite.
#[derive(Debug)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

pub fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(message.into()))
}

/// Message d'erreur complet, causes comprises. Les erreurs système les plus courantes sont dites
/// en français ; les autres gardent le texte du système.
pub fn describe(error: &anyhow::Error) -> String {
    let mut denied = false;
    let mut text = error
        .chain()
        .map(|cause| match cause.downcast_ref::<io::Error>() {
            Some(io) if io.kind() == io::ErrorKind::NotFound => "introuvable".to_owned(),
            Some(io) if io.kind() == io::ErrorKind::PermissionDenied => {
                denied = true;
                "accès refusé".to_owned()
            }
            Some(io) if io.kind() == io::ErrorKind::AlreadyExists => "existe déjà".to_owned(),
            // Une erreur peut citer une valeur de la stack ou du serveur.
            _ => am_protocol::visible_lines(&cause.to_string()),
        })
        .collect::<Vec<_>>()
        .join(" : ");
    if denied {
        text.push_str(
            "\ndrakbase-stack pilote Docker et écrit dans /etc et /var/lib : lancez-le avec sudo.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_outcome() {
        use Outcome::*;
        assert_eq!(Done.worst(PermissionRequired), PermissionRequired);
        assert_eq!(PermissionRequired.worst(Failed), Failed);
        assert_eq!(Failed.worst(Done), Failed);
        assert_eq!(Done.worst(Done), Done);
        assert_eq!(
            [
                Done.code(),
                Failed.code(),
                Outcome::USAGE,
                PermissionRequired.code()
            ],
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn describes_io_errors_in_french() {
        let error = anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied))
            .context("écriture de /etc/drakbase-stack/config.json");
        let text = describe(&error);
        assert!(text.starts_with("écriture de /etc/drakbase-stack/config.json : accès refusé"));
        assert!(text.contains("sudo"));
    }
}
