//! `auto on|off` : les unités systemd de l'agent (docs/PROTOCOLE.md, « Autres commandes ») :
//! - un minuteur, une fois par jour avec un décalage aléatoire d'une heure au plus et un
//!   rattrapage après un arrêt, qui lance `self-update`, puis `check` pour chaque stack : il
//!   vérifie et prévient, sans rien installer ;
//! - une unité `drakbase-stack-request@<app>.path` par stack dont la version qui tourne a un
//!   dossier de contrôle (`app_control`), qui lance `request <app>` dès que l'interface de l'app
//!   dépose une demande.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::files;
use crate::outcome::Outcome;
use crate::paths::{ENV_SYSTEMCTL, Paths, program_from_env};
use crate::state;
use crate::update::{self, Agent};

pub const SERVICE: &str = "drakbase-stack.service";
pub const TIMER: &str = "drakbase-stack.timer";
pub const REQUEST_PATH: &str = "drakbase-stack-request@.path";
pub const REQUEST_SERVICE: &str = "drakbase-stack-request@.service";

/// L'unité qui surveille les demandes d'une stack.
pub fn request_unit(app: &str) -> String {
    format!("drakbase-stack-request@{app}.path")
}

pub fn is_enabled(paths: &Paths) -> bool {
    paths.systemd_dir.join(TIMER).is_file()
}

/// Service lancé par le minuteur. `self-update` peut échouer (serveur injoignable, exécutable
/// sans clé embarquée) sans empêcher la vérification des stacks : d'où le `-`. `check` ne fait
/// que vérifier : rien ne s'installe sans qu'une personne le demande.
pub fn service_unit(exe: &Path) -> String {
    let exe = systemd_quote(&exe.to_string_lossy());
    format!(
        "# Écrit par drakbase-stack auto on ; drakbase-stack auto off le retire.
[Unit]
Description=Mise à jour de drakbase-stack et vérification des stacks Docker de DrakBase
Wants=network-online.target
After=network-online.target docker.service

[Service]
Type=oneshot
ExecStart=-{exe} self-update
ExecStart={exe} check
"
    )
}

pub fn timer_unit() -> String {
    "# Écrit par drakbase-stack auto on ; drakbase-stack auto off le retire.
[Unit]
Description=Vérification quotidienne des stacks Docker de DrakBase

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
"
    .to_owned()
}

/// Surveille le dossier des demandes d'une stack (`%i` : son identifiant) : non vide, il lance
/// le service qui les traite, et qui le vide.
pub fn request_path_unit(state_dir: &Path) -> String {
    let dir =
        systemd_quote(&state_dir.join("%i/control/request").to_string_lossy()).replace("%%i", "%i");
    format!(
        "# Écrit par drakbase-stack auto on ; drakbase-stack auto off le retire.
[Unit]
Description=Demandes de mise à jour de la stack %i, venues de son interface

[Path]
DirectoryNotEmpty={dir}

[Install]
WantedBy=paths.target
"
    )
}

pub fn request_service_unit(exe: &Path) -> String {
    let exe = systemd_quote(&exe.to_string_lossy());
    format!(
        "# Écrit par drakbase-stack auto on ; drakbase-stack auto off le retire.
[Unit]
Description=Demande de mise à jour de la stack %i
Wants=network-online.target
After=network-online.target docker.service

[Service]
Type=oneshot
ExecStart={exe} request %i
"
    )
}

/// Accorde l'unité des demandes d'une stack à la version qui tourne : activée si elle a un
/// dossier de contrôle, désactivée sinon. Sans effet tant que les unités de l'agent ne sont pas
/// installées (`auto on`).
pub fn sync_stack(agent: &Agent, app: &str, has_control: bool) -> Result<()> {
    if !is_enabled(&agent.paths) {
        return Ok(());
    }
    let systemctl = program_from_env(ENV_SYSTEMCTL, "systemctl");
    let unit = request_unit(app);
    if has_control {
        return systemctl_run(&systemctl, &["enable", "--now", &unit]);
    }
    // Le plus souvent, cette unité n'a jamais été activée : ce que systemctl en dit alors n'est
    // pas un échec de l'opération en cours.
    let _ = systemctl_run(&systemctl, &["disable", "--now", &unit]);
    Ok(())
}

/// Refuse un exécutable qu'un autre compte pourrait remplacer : le minuteur le lance chaque jour
/// en root. Lui-même et chacun de ses dossiers appartiennent à root, ou au compte `owner` (celui
/// du dossier des unités), et ne sont ouverts en écriture ni au groupe ni à tous, sauf un dossier
/// à suppression restreinte (`/tmp`), où nul ne retire le fichier d'un autre.
fn check_exe(exe: &Path, owner: u32) -> Result<()> {
    for path in exe.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        let metadata =
            fs::metadata(path).with_context(|| format!("droits de {}", path.display()))?;
        let mode = metadata.permissions().mode();
        let sticky_dir = metadata.is_dir() && mode & 0o1000 != 0;
        let foreign = metadata.uid() != 0 && metadata.uid() != owner;
        if foreign || (mode & 0o022 != 0 && !sticky_dir) {
            bail!(
                "{} peut être remplacé par un autre compte que root ({}) : le minuteur le lancerait chaque jour en root. Installez drakbase-stack dans /usr/local/bin, puis relancez auto on.",
                exe.display(),
                path.display()
            );
        }
    }
    Ok(())
}

/// Un argument de ligne de commande systemd : `%` doublé, guillemets s'il le faut.
fn systemd_quote(value: &str) -> String {
    let value = value.replace('%', "%%");
    if value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'))
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value
    }
}

pub fn run(agent: &Agent, on: bool) -> Result<Outcome> {
    let dir = &agent.paths.systemd_dir;
    let systemctl = program_from_env(ENV_SYSTEMCTL, "systemctl");
    if on {
        let exe = env::current_exe().context("chemin de drakbase-stack")?;
        let apps = state::list(&agent.paths)?;
        files::create_dir(dir)?;
        check_exe(&exe, fs::metadata(dir)?.uid())?;
        let units = [
            (SERVICE, service_unit(&exe)),
            (TIMER, timer_unit()),
            (REQUEST_PATH, request_path_unit(&agent.paths.state_dir)),
            (REQUEST_SERVICE, request_service_unit(&exe)),
        ];
        for (name, text) in &units {
            files::replace(&dir.join(name), text.as_bytes(), 0o644)?;
        }
        let enabled = systemctl_run(&systemctl, &["daemon-reload"])
            .and_then(|()| systemctl_run(&systemctl, &["enable", "--now", TIMER]))
            // Chaque stack installée : son unité des demandes, si sa version qui tourne a un
            // dossier de contrôle.
            .and_then(|()| {
                apps.iter()
                    .try_for_each(|app| update::settle_installed(agent, app))
            });
        if let Err(error) = enabled {
            // Sans systemd qui les prenne en charge, les unités ne restent pas.
            for (name, _) in &units {
                let _ = files::remove_file(&dir.join(name));
            }
            return Err(error);
        }
        println!(
            "Vérification quotidienne activée : chaque jour, à une heure tirée au hasard, drakbase-stack se met à jour lui-même, puis vérifie les stacks, sans rien installer (systemctl list-timers {TIMER})."
        );
        println!(
            "Les demandes de mise à jour validées dans l'interface d'une app sont traitées dès leur arrivée."
        );
    } else {
        if dir.join(TIMER).exists() {
            systemctl_run(&systemctl, &["disable", "--now", TIMER])?;
            for app in state::list(&agent.paths)? {
                let _ = systemctl_run(&systemctl, &["disable", "--now", &request_unit(&app)]);
            }
        }
        for name in [TIMER, SERVICE, REQUEST_PATH, REQUEST_SERVICE] {
            files::remove_file(&dir.join(name))?;
        }
        systemctl_run(&systemctl, &["daemon-reload"])?;
        println!(
            "Vérification quotidienne coupée : les mises à jour ne se font plus que par sudo drakbase-stack update."
        );
    }
    Ok(Outcome::Done)
}

fn systemctl_run(program: &OsString, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                anyhow!("systemctl introuvable : les mises à jour automatiques demandent systemd")
            } else {
                anyhow::Error::new(error).context("lancement de systemctl")
            }
        })?;
    if !output.status.success() {
        bail!(
            "systemctl {} a échoué : {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        let service = service_unit(Path::new("/usr/local/bin/drakbase-stack"));
        assert!(service.contains("ExecStart=-/usr/local/bin/drakbase-stack self-update\n"));
        assert!(service.contains("ExecStart=/usr/local/bin/drakbase-stack check\n"));
        assert!(!service.contains(" update"));
        let path = request_path_unit(Path::new("/var/lib/drakbase-stack"));
        assert!(
            path.contains("DirectoryNotEmpty=/var/lib/drakbase-stack/%i/control/request\n"),
            "{path}"
        );
        let request = request_service_unit(Path::new("/usr/local/bin/drakbase-stack"));
        assert!(request.contains("ExecStart=/usr/local/bin/drakbase-stack request %i\n"));
        assert!(service.contains("Type=oneshot"));
        let timer = timer_unit();
        for line in [
            "OnCalendar=daily",
            "RandomizedDelaySec=1h",
            "Persistent=true",
            "WantedBy=timers.target",
        ] {
            assert!(timer.contains(line), "{line}");
        }
    }

    #[test]
    fn an_executable_others_can_replace_is_refused() {
        use std::fs::Permissions;
        let dir = tempfile::tempdir().unwrap();
        let owner = fs::metadata(dir.path()).unwrap().uid();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
        let exe = bin.join("drakbase-stack");
        fs::write(&exe, "").unwrap();
        fs::set_permissions(&exe, Permissions::from_mode(0o755)).unwrap();
        check_exe(&exe, owner).unwrap();

        // Un dossier ouvert à tous en écriture : n'importe qui y remplace l'exécutable.
        fs::set_permissions(&bin, Permissions::from_mode(0o777)).unwrap();
        let error = check_exe(&exe, owner).unwrap_err().to_string();
        assert!(error.contains("peut être remplacé"), "{error}");
        // Le même, à suppression restreinte comme /tmp : nul n'y retire le fichier d'un autre.
        fs::set_permissions(&bin, Permissions::from_mode(0o1777)).unwrap();
        check_exe(&exe, owner).unwrap();

        // L'exécutable lui-même, ouvert en écriture au groupe.
        fs::set_permissions(&exe, Permissions::from_mode(0o775)).unwrap();
        assert!(check_exe(&exe, owner).is_err());
        // Un exécutable d'un autre compte que root et que celui des unités.
        fs::set_permissions(&exe, Permissions::from_mode(0o755)).unwrap();
        assert!(check_exe(&exe, owner.wrapping_add(1)).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(systemd_quote("/usr/local/bin/x"), "/usr/local/bin/x");
        assert_eq!(systemd_quote("/a b/x"), "\"/a b/x\"");
        assert_eq!(systemd_quote("/a%b"), "/a%%b");
    }
}
