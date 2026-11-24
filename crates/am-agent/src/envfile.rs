//! `.env` d'une stack : créé à partir de `.env.example`, puis seulement complété. Les variables
//! nouvelles d'une version y sont ajoutées avec leur valeur par défaut ; celles qui y sont déjà ne
//! sont jamais modifiées (docs/PROTOCOLE.md, étape 6 de la mise à jour).
//!
//! La lecture se fait ligne par ligne, avec la règle de `compose::env_variable`. Un `.env.example`
//! n'a que des lignes qu'elle lit comme Docker Compose : l'archive qui en a d'autres est refusée
//! (`compose::check_env_example`).

use std::collections::BTreeSet;

use am_protocol::stack::compose::{env_variable as variable, env_variable_as_written};

/// Résultat de la fusion : le texte du nouveau `.env` et les variables ajoutées.
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    pub added: Vec<String>,
}

/// Fusionne `example` (le `.env.example` de la version) dans `current` (le `.env` de
/// l'utilisateur, `None` s'il n'existe pas encore).
pub fn merge(current: Option<&str>, example: Option<&str>) -> Merged {
    let example = example.unwrap_or_default();
    let Some(current) = current else {
        // Première installation : le .env est la copie de .env.example, commentaires compris.
        let mut text = example.to_owned();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let added = example.lines().filter_map(variable).map(str::to_owned);
        return Merged {
            text,
            added: dedup(added),
        };
    };

    // Une variable que l'utilisateur a mise en commentaire compte comme connue : elle ne
    // revient pas.
    let mut known: BTreeSet<&str> = current
        .lines()
        .filter_map(|line| {
            let content = line.trim_start();
            if content.starts_with('#') {
                variable(content.trim_start_matches('#'))
            } else {
                // Hors commentaire, Docker Compose lit aussi `NOM: valeur` : la variable est là.
                env_variable_as_written(line)
            }
        })
        .collect();
    let mut additions: Vec<&str> = Vec::new();
    let mut added = Vec::new();
    // Commentaires qui précèdent une variable : ils l'accompagnent si elle est ajoutée.
    let mut comments: Vec<&str> = Vec::new();
    for line in example.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            comments.push(line);
            continue;
        }
        if let Some(name) = variable(line)
            && known.insert(name)
        {
            additions.append(&mut comments);
            additions.push(line);
            added.push(name.to_owned());
        }
        comments.clear();
    }

    let mut text = current.to_owned();
    if !additions.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        for line in additions {
            text.push_str(line);
            text.push('\n');
        }
    }
    Merged { text, added }
}

/// Retire de `text` les lignes qui donnent une valeur à l'une des variables `names` (pas celles
/// en commentaire). Les autres lignes restent telles quelles. Rend le texte et les variables
/// retirées, dans l'ordre de `names`.
pub fn remove(text: &str, names: &[String]) -> (String, Vec<String>) {
    let mut kept = String::with_capacity(text.len());
    let mut removed = BTreeSet::new();
    for line in text.split_inclusive('\n') {
        match variable(line).filter(|name| names.iter().any(|wanted| wanted == name)) {
            Some(name) => {
                removed.insert(name.to_owned());
            }
            None => kept.push_str(line),
        }
    }
    let removed = names
        .iter()
        .filter(|name| removed.contains(name.as_str()))
        .cloned()
        .collect();
    (kept, removed)
}

fn dedup(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    names.filter(|name| seen.insert(name.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_named_variables() {
        let names = [
            "TZ".to_owned(),
            "HM_READ_ONLY".to_owned(),
            "ABSENT".to_owned(),
        ];
        let text = "PORT=9000\n# Fuseau horaire\nTZ=Europe/Berlin\n# TZ=UTC\nexport HM_READ_ONLY=1\nAUTRE=2";
        let (kept, removed) = remove(text, &names);
        assert_eq!(kept, "PORT=9000\n# Fuseau horaire\n# TZ=UTC\nAUTRE=2");
        assert_eq!(removed, ["TZ", "HM_READ_ONLY"]);
        assert_eq!(remove("PORT=1", &names), ("PORT=1".to_owned(), vec![]));
    }

    #[test]
    fn first_install_copies_the_example() {
        let merged = merge(
            None,
            Some("# Port de l'interface\nPORT=8080\nTZ=Europe/Paris"),
        );
        assert_eq!(
            merged.text,
            "# Port de l'interface\nPORT=8080\nTZ=Europe/Paris\n"
        );
        assert_eq!(merged.added, ["PORT", "TZ"]);
        assert_eq!(
            merge(None, None),
            Merged {
                text: String::new(),
                added: vec![]
            }
        );
    }

    #[test]
    fn update_only_adds_new_variables_with_their_comments() {
        let current = "# Port de l'interface\nPORT=9000\n";
        let example = "# Port de l'interface\nPORT=8080\n\n# Fuseau horaire\n# (tz database)\nTZ=Europe/Paris\n# sans variable\n\nHM_READ_ONLY=0";
        let merged = merge(Some(current), Some(example));
        assert_eq!(
            merged.text,
            "# Port de l'interface\nPORT=9000\n\n# Fuseau horaire\n# (tz database)\nTZ=Europe/Paris\nHM_READ_ONLY=0\n"
        );
        assert_eq!(merged.added, ["TZ", "HM_READ_ONLY"]);

        // Rien de nouveau : le fichier reste tel quel, même sans saut de ligne final.
        let unchanged = merge(Some("PORT=1"), Some("PORT=8080\n"));
        assert_eq!(unchanged.text, "PORT=1");
        assert!(unchanged.added.is_empty());
        // Sans saut de ligne final, l'ajout commence sur une nouvelle ligne.
        let merged = merge(Some("PORT=1"), Some("TZ=UTC"));
        assert_eq!(merged.text, "PORT=1\n\nTZ=UTC\n");
        // Une variable que l'utilisateur a mise en commentaire ne revient pas.
        let merged = merge(
            Some("#PORT=1\n# TZ=UTC\n"),
            Some("PORT=8080\nTZ=Europe/Paris\n"),
        );
        assert!(merged.added.is_empty());
        // Une variable écrite `NOM: valeur`, que Docker Compose lit aussi, est déjà là : sa
        // valeur par défaut, ajoutée en fin de fichier, l'emporterait.
        let merged = merge(Some("PORT: 9000\n"), Some("PORT=8080\nTZ=Europe/Paris\n"));
        assert_eq!(merged.added, ["TZ"]);
        assert_eq!(merged.text, "PORT: 9000\n\nTZ=Europe/Paris\n");
        // Un commentaire de cette forme n'est qu'une phrase.
        let merged = merge(Some("# TZ: le fuseau horaire\n"), Some("TZ=UTC\n"));
        assert_eq!(merged.added, ["TZ"]);
    }
}
