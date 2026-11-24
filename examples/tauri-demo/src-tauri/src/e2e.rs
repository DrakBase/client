//! Mode d'essai de bout en bout, piloté par les scripts de `scripts/` : options lues sur la ligne
//! de commande, et journal des étapes dans un fichier, une ligne JSON par événement. Une vraie app
//! n'en a pas besoin.
//!
//! Les options passent par la ligne de commande et non par l'environnement : sur macOS, Tauri
//! relance l'app par LaunchServices (`open -n … --args`), qui transmet les arguments mais pas les
//! variables d'environnement.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

#[derive(Debug, Default)]
pub struct LaunchOptions {
    /// `--journal <fichier>` : active le mode d'essai et dit où écrire le journal.
    journal: Option<PathBuf>,
    /// `--auto-update` : cherche, installe et relance sans intervention.
    auto_update: bool,
    /// `--channel stable|beta` : enregistré comme si l'utilisateur l'avait choisi.
    channel: Option<String>,
    /// `--stats on|off` : enregistré comme si l'utilisateur l'avait choisi.
    share_statistics: Option<bool>,
}

impl LaunchOptions {
    /// Lit les options ; tout argument inconnu est ignoré.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut options = Self::default();
        let mut args = args.into_iter().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--journal" => options.journal = args.next().map(PathBuf::from),
                "--auto-update" => options.auto_update = true,
                "--channel" => {
                    options.channel = args
                        .next()
                        .filter(|value| value == "stable" || value == "beta");
                }
                "--stats" => {
                    options.share_statistics = match args.next().as_deref() {
                        Some("on") => Some(true),
                        Some("off") => Some(false),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        options
    }

    /// Vrai en mode d'essai. Les commandes réservées aux essais (`--quit` envoyé par une seconde
    /// instance) ne sont suivies que dans ce mode.
    pub fn is_e2e(&self) -> bool {
        self.journal.is_some()
    }

    /// Ce que l'interface doit savoir.
    pub fn to_json(&self) -> Value {
        json!({
            "e2e": self.is_e2e(),
            "autoUpdate": self.auto_update,
            "channel": self.channel,
            "shareStatistics": self.share_statistics,
        })
    }

    /// Ajoute une ligne au journal, avec l'heure en millisecondes et le PID. Sans `--journal`, ne
    /// fait rien. L'ancienne et la nouvelle instance écrivent dans le même fichier : chaque ligne
    /// part en une seule écriture, en ajout.
    pub fn journal(&self, event: &str, data: Value) {
        let Some(path) = &self.journal else {
            return;
        };
        let mut entry = Map::new();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        entry.insert("t".into(), json!(millis));
        entry.insert("pid".into(), json!(std::process::id()));
        entry.insert("event".into(), json!(event));
        match data {
            Value::Object(fields) => entry.extend(fields),
            Value::Null => {}
            other => {
                entry.insert("data".into(), other);
            }
        }
        let mut line = Value::Object(entry).to_string();
        line.push('\n');
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(error) = written {
            eprintln!("journal {} : {error}", path.display());
        }
    }
}

#[tauri::command]
pub fn launch_options(options: tauri::State<'_, LaunchOptions>) -> Value {
    options.to_json()
}

#[tauri::command]
pub fn journal(options: tauri::State<'_, LaunchOptions>, event: String, data: Value) {
    options.journal(&event, data);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> LaunchOptions {
        LaunchOptions::from_args(
            std::iter::once("tauri-demo")
                .chain(args.iter().copied())
                .map(str::to_owned),
        )
    }

    #[test]
    fn options_are_read_from_the_command_line() {
        let options = parse(&[
            "--journal",
            "/tmp/j.log",
            "--auto-update",
            "--channel",
            "beta",
            "--stats",
            "off",
            "-psn_0_1234",
        ]);
        assert!(options.is_e2e());
        assert_eq!(
            options.to_json(),
            json!({ "e2e": true, "autoUpdate": true, "channel": "beta", "shareStatistics": false })
        );
    }

    #[test]
    fn unknown_values_are_ignored() {
        let options = parse(&["--channel", "nightly", "--stats", "peut-être"]);
        assert!(!options.is_e2e());
        assert_eq!(
            options.to_json(),
            json!({ "e2e": false, "autoUpdate": false, "channel": null, "shareStatistics": null })
        );
    }

    #[test]
    fn journal_lines_are_appended_as_json() {
        let dir = std::env::temp_dir().join(format!("tauri-demo-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("journal.log");
        let options = parse(&["--journal", path.to_str().unwrap()]);
        options.journal("start", json!({ "version": "0.1.0" }));
        options.journal("phase", json!("checking"));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "start");
        assert_eq!(lines[0]["version"], "0.1.0");
        assert_eq!(lines[0]["pid"], std::process::id());
        assert_eq!(lines[1]["data"], "checking");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
