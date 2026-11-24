//! Commandes autour d'une stack installée : `rollback`, `restart`, `uninstall`, `status` et
//! `logs`.

use std::fs;

use am_protocol::stack::compose::{self, CreatedVolume};
use am_protocol::stack::manifest::StackManifest;
use am_protocol::{ServerUrl, Version, visible};
use anyhow::{Result, bail};

use crate::auto;
use crate::config::Config;
use crate::docker::ServiceStatus;
use crate::files;
use crate::outcome::{Outcome, describe, usage};
use crate::prompt;
use crate::state::{self, StackState};
use crate::update::{
    Agent, OwnedProject, cleanup, ensure_env, examine_kept, kept_refused, list_item,
    mark_interrupted, refused_way_out, relaunch, release_project, say, show_logs,
};
use crate::{LogsArgs, RestartArgs, RollbackArgs, UninstallArgs};

/// `rollback <app> [--accept]` : relance la version précédente et écarte celle qu'il quitte.
/// Après un échec laissé en l'état (`rollback: manual`), la version à relancer est la dernière
/// saine. La version relancée est d'abord contrôlée avec le `.env` courant, comme par `restart` :
/// une permission nouvelle arrête jusqu'à `--accept`.
pub fn rollback(agent: &Agent, args: RollbackArgs) -> Result<Outcome> {
    let app = args.app.as_str();
    let stack = agent.paths.stack(app);
    let _lock = files::lock(&stack.lock_file(), app)?;
    let mut state = StackState::require(&agent.paths, app)?;
    mark_interrupted(&agent.paths, &mut state)?;
    agent.docker.prerequisites()?;

    if let Some(failed) = state.failed.clone() {
        let Some(installed) = state.installed.clone() else {
            bail!(
                "{app} n'a aucune version saine à relancer : sudo drakbase-stack uninstall {app} pour tout retirer, ou sudo drakbase-stack update {app} --retry pour réessayer la {}",
                failed.version
            );
        };
        let Some(manifest) = checked(
            agent,
            &mut state,
            &installed.version,
            args.accept,
            "rollback",
        )?
        else {
            return Ok(Outcome::PermissionRequired);
        };
        say(
            app,
            format!(
                "retour à la version {} (la {} n'est pas devenue saine)",
                installed.version, failed.version
            ),
        );
        if !relaunched(agent, app, &installed.version, &manifest) {
            return Ok(Outcome::Failed);
        }
        state.failed = None;
        state.reject(&failed.version);
        state.save(&agent.paths)?;
        cleanup(agent, &state, &[failed]);
        say(
            app,
            format!("version {} relancée, saine", installed.version),
        );
        return Ok(Outcome::Done);
    }

    let (Some(installed), Some(previous)) = (state.installed.clone(), state.previous.clone())
    else {
        bail!("{app} n'a pas de version précédente gardée sur cette machine");
    };
    let Some(manifest) = checked(
        agent,
        &mut state,
        &previous.version,
        args.accept,
        "rollback",
    )?
    else {
        return Ok(Outcome::PermissionRequired);
    };
    say(
        app,
        format!(
            "retour à la version {} ; la {} sera écartée",
            previous.version, installed.version
        ),
    );
    if !relaunched(agent, app, &previous.version, &manifest) {
        return Ok(Outcome::Failed);
    }
    state.reject(&installed.version);
    let mut relaunched_record = previous;
    relaunched_record.since = state::now();
    state.installed = Some(relaunched_record);
    state.previous = None;
    state.save(&agent.paths)?;
    cleanup(agent, &state, std::slice::from_ref(&installed));
    say(
        app,
        format!(
            "version {} relancée, saine ; la {} est écartée (drakbase-stack update {app} --retry pour la reprendre)",
            state
                .installed_version()
                .map(Version::to_string)
                .unwrap_or_default(),
            installed.version
        ),
    );
    Ok(Outcome::Done)
}

/// `restart <app> [--accept]` : relance la version installée avec le `.env` actuel, par exemple
/// après un réglage changé à la main. Le compose est d'abord contrôlé avec ce `.env` : une
/// permission nouvelle (un autre port…) arrête, comme une mise à jour, jusqu'à `--accept`.
pub fn restart(agent: &Agent, args: RestartArgs) -> Result<Outcome> {
    let app = args.app.as_str();
    let stack = agent.paths.stack(app);
    let _lock = files::lock(&stack.lock_file(), app)?;
    let mut state = StackState::require(&agent.paths, app)?;
    mark_interrupted(&agent.paths, &mut state)?;
    agent.docker.prerequisites()?;
    if let Some(failed) = &state.failed {
        bail!(
            "la version {} de {app} n'est pas devenue saine : {}",
            failed.version,
            crate::update::failed_way_out(agent, &state, &failed.version)
        );
    }
    let Some(version) = state.installed_version().cloned() else {
        bail!("{app} n'a pas de version installée sur cette machine");
    };
    let Some(manifest) = checked(agent, &mut state, &version, args.accept, "restart")? else {
        return Ok(Outcome::PermissionRequired);
    };

    say(
        app,
        format!(
            "relance de la version {version} avec les réglages de {}",
            stack.env_file().display()
        ),
    );
    if !relaunched(agent, app, &version, &manifest) {
        return Ok(Outcome::Failed);
    }
    say(app, format!("version {version} relancée, saine"));
    Ok(Outcome::Done)
}

/// Contrôle une version gardée avant sa relance par `command` (`restart` ou `rollback`), avec le
/// `.env` courant, et fait accepter ses permissions nouvelles par `--accept`. Rend son
/// `stack.json`, ou `None` quand une permission reste à accepter : rien n'est alors relancé.
fn checked(
    agent: &Agent,
    state: &mut StackState,
    version: &Version,
    accept: bool,
    command: &str,
) -> Result<Option<StackManifest>> {
    let app = state.app.clone();
    let kept = match examine_kept(agent, state, version) {
        Ok(kept) => kept,
        // Refusée par cet agent : dire ce qui reste possible, et non une commande qui échouera.
        Err(error) => {
            let Some(with_env) = kept_refused(&error) else {
                return Err(error);
            };
            let stack = agent.paths.stack(&app);
            let way_out = if command == "rollback" && state.failed.is_none() {
                // La version installée tourne toujours : seule la précédente est perdue.
                let running = state
                    .installed_version()
                    .map(Version::to_string)
                    .unwrap_or_default();
                format!(
                    "La version {version} ne peut plus être relancée ; la {running} continue de tourner."
                )
            } else {
                let failed = state.failed.as_ref().map(|failed| &failed.version);
                refused_way_out(&stack, &app, failed, with_env, command)
            };
            bail!("{}\n{way_out}", describe(&error));
        }
    };
    if !kept.new_permissions.is_empty() {
        if command == "restart" {
            println!("{app} : les réglages de .env demandent de nouvelles permissions :");
        } else {
            println!(
                "{app} : avec les réglages de .env, la version {version} demande de nouvelles permissions :"
            );
        }
        for permission in &kept.new_permissions {
            list_item(permission);
        }
        if !accept {
            println!("Pour les accepter : sudo drakbase-stack {command} {app} --accept");
            return Ok(None);
        }
        println!("Permissions acceptées (--accept).");
        state.accept(
            kept.new_permissions
                .iter()
                .map(|permission| permission.key()),
        );
        state.save(&agent.paths)?;
    }
    Ok(Some(kept.manifest))
}

/// Relance une version déjà contrôlée ; en cas d'échec, l'explique avec la fin du journal.
fn relaunched(agent: &Agent, app: &str, version: &Version, manifest: &StackManifest) -> bool {
    match relaunch(agent, app, version, manifest) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "{app} : la version {version} ne devient pas saine : {}",
                describe(&error)
            );
            let project = release_project(agent, app, version);
            if project.dir.is_dir() {
                show_logs(&agent.docker, &project.project());
            }
            false
        }
    }
}

/// `uninstall <app> [--delete-data]` : conteneurs, réseaux, images et fichiers de la stack.
/// Les volumes et le `.env` restent, sauf avec `--delete-data` et une confirmation.
pub fn uninstall(agent: &Agent, args: UninstallArgs) -> Result<Outcome> {
    let app = args.app.as_str();
    let stack = agent.paths.stack(app);
    let _lock = files::lock(&stack.lock_file(), app)?;
    let state = StackState::require(&agent.paths, app)?;
    agent.docker.prerequisites()?;
    if args.delete_data && !agent.interactive {
        return Err(usage(format!(
            "--delete-data efface les données de {app} et demande une confirmation : lancez la commande dans un terminal"
        )));
    }

    // Le compose de la version qui tourne, à défaut celui d'une autre version gardée.
    let running = state
        .kept()
        .map(|record| release_project(agent, app, &record.version))
        .find(|project| project.dir.is_dir());
    // Docker Compose exige le fichier des réglages, même pour lire le compose. Créé ici parce
    // qu'il avait été effacé à la main, il ne reste pas si la désinstallation ne se fait pas.
    let env_created = running.is_some() && !stack.env_file().exists();
    if running.is_some() {
        ensure_env(&stack)?;
    }

    if args.delete_data {
        let confirmed = confirm_delete_data(agent, app, running.as_ref());
        if !matches!(confirmed, Ok(true)) && env_created {
            let _ = files::remove_file(&stack.env_file());
        }
        if !confirmed? {
            println!("Abandon : rien n'a été supprimé.");
            return Ok(Outcome::Failed);
        }
    }

    match &running {
        Some(project) => {
            say(app, "arrêt et suppression des conteneurs");
            agent.docker.down(&project.project(), args.delete_data)?;
        }
        None => eprintln!("{app} : aucune version sur la machine, rien à arrêter"),
    }
    let images: Vec<String> = state.kept_images().into_iter().map(str::to_owned).collect();
    agent.docker.remove_images(&images);
    if let Err(error) = auto::sync_stack(agent, app, false) {
        eprintln!("{app} : attention : {}", describe(&error));
    }

    files::remove_dir(&stack.releases())?;
    files::remove_dir(&stack.control_dir())?;
    files::remove_dir(&stack.check_root())?;
    files::remove_file(&stack.check_env())?;
    files::remove_file(&stack.env_next())?;
    files::remove_file(&stack.status_temp())?;
    files::remove_file(&stack.state_file())?;
    if args.delete_data {
        files::remove_file(&stack.env_file())?;
    }
    files::remove_file(&stack.lock_file())?;
    let _ = fs::remove_dir(&stack.dir);
    if args.delete_data {
        say(app, "désinstallée, données comprises");
    } else {
        say(
            app,
            format!(
                "désinstallée ; ses volumes restent, et ses réglages dans {} pour une nouvelle installation",
                stack.env_file().display()
            ),
        );
    }
    Ok(Outcome::Done)
}

/// Annonce ce que `--delete-data` supprimera, puis demande le nom de l'app. `true` si la
/// personne confirme.
fn confirm_delete_data(
    agent: &Agent,
    app: &str,
    running: Option<&OwnedProject<'_>>,
) -> Result<bool> {
    let volumes = match running {
        Some(project) => compose::created_volumes(&agent.docker.config(&project.project())?, app),
        None => Vec::new(),
    };
    println!("{}", delete_data_notice(app, &volumes));
    let answer = prompt::ask(&format!("Tapez « {app} » pour confirmer : "))?;
    Ok(answer == app)
}

/// Ce que `uninstall --delete-data` annonce avant sa confirmation : les volumes qui partiront,
/// par leur nom, et ceux dont le nom fixe peut être partagé avec une autre stack. Sans volume
/// nommé à citer, la première phrase suffit.
fn delete_data_notice(app: &str, volumes: &[CreatedVolume]) -> String {
    let mut notice = format!(
        "--delete-data supprime les volumes de {app}, donc ses données, et ses réglages (.env). Rien ne pourra être récupéré."
    );
    if volumes.is_empty() {
        return notice;
    }
    let names: Vec<&str> = volumes.iter().map(|volume| volume.name.as_str()).collect();
    notice.push_str(&format!(
        "\nVolumes supprimés : {}, et les volumes sans nom de ses conteneurs.",
        names.join(", ")
    ));
    let shared: Vec<&str> = volumes
        .iter()
        .filter(|volume| volume.shared)
        .map(|volume| volume.name.as_str())
        .collect();
    match shared.as_slice() {
        [] => {}
        [one] => notice.push_str(&format!(
            "\nAttention : le volume {one} a un nom fixe. Une autre stack qui s'en sert perdrait aussi ses données."
        )),
        [first @ .., last] => notice.push_str(&format!(
            "\nAttention : les volumes {} et {last} ont un nom fixe. Une autre stack qui s'en sert perdrait aussi ses données.",
            first.join(", ")
        )),
    }
    notice
}

/// Le projet Compose de la version qui tourne : celle restée en échec, sinon l'installée.
fn running_project<'a>(agent: &Agent, state: &'a StackState) -> Option<OwnedProject<'a>> {
    [&state.failed, &state.installed]
        .into_iter()
        .flatten()
        .map(|record| release_project(agent, &state.app, &record.version))
        .find(|project| project.dir.is_dir())
}

/// `logs <app>` : journal des services de la version qui tourne.
pub fn logs(agent: &Agent, args: LogsArgs) -> Result<Outcome> {
    let app = args.app.as_str();
    let state = StackState::require(&agent.paths, app)?;
    agent.docker.prerequisites()?;
    let Some(project) = running_project(agent, &state) else {
        bail!("{app} n'a aucune version sur cette machine");
    };
    ensure_env(&agent.paths.stack(app))?;
    agent
        .docker
        .logs(&project.project(), args.follow, args.tail)?;
    Ok(Outcome::Done)
}

/// `status` : l'agent, le minuteur, et chaque stack avec ses versions et ses services.
pub fn status(agent: &Agent) -> Result<Outcome> {
    let config = Config::load(&agent.paths)?;
    let server = config
        .as_ref()
        .map(|config| config.server.clone())
        .unwrap_or_else(ServerUrl::production);
    println!("drakbase-stack {}", env!("CARGO_PKG_VERSION"));
    let stats = match &config {
        Some(config) if !config.stats => " (statistiques coupées)",
        _ => "",
    };
    println!("Serveur : {server}{stats}");
    println!(
        "Vérification quotidienne : {}",
        if auto::is_enabled(&agent.paths) {
            "activée (minuteur drakbase-stack.timer), demandes des interfaces traitées dès leur arrivée"
        } else {
            "coupée (sudo drakbase-stack auto on pour l'activer)"
        }
    );
    let apps = state::list(&agent.paths)?;
    if apps.is_empty() {
        println!("Aucune stack installée.");
        return Ok(Outcome::Done);
    }
    let docker = match agent.docker.prerequisites() {
        Ok(_) => true,
        Err(error) => {
            eprintln!("attention : {}", describe(&error));
            false
        }
    };
    for app in apps {
        let state = StackState::require(&agent.paths, &app)?;
        println!();
        match state.installed_version() {
            Some(installed) => println!("{app} {installed} (canal {})", state.channel),
            None => println!("{app} : aucune version saine (canal {})", state.channel),
        }
        if let Some(previous) = &state.previous {
            println!(
                "  précédente : {} (drakbase-stack rollback {app})",
                previous.version
            );
        }
        if let Some(failed) = &state.failed {
            println!(
                "  en échec : {}, conteneurs laissés en l'état (drakbase-stack rollback {app}, qui dit ce qui reste possible si la version d'avant ne peut plus être relancée)",
                failed.version
            );
        }
        if let Some(available) = &state.available {
            println!(
                "  disponible : {} ({}), à installer depuis l'interface de l'app ou par sudo drakbase-stack update {app}{}",
                available.version,
                available.channel,
                if available.new_permissions.is_empty() {
                    ""
                } else {
                    " --accept (permissions nouvelles)"
                }
            );
        }
        if let Some(checked_at) = &state.checked_at {
            println!("  dernière vérification : {checked_at}");
        }
        if let Some(operation) = &state.operation {
            println!("  dernière demande : {}", visible(&operation.message));
        }
        if !state.rejected.is_empty() {
            let rejected: Vec<String> = state.rejected.iter().map(Version::to_string).collect();
            println!(
                "  écartées : {} (drakbase-stack update {app} --retry)",
                rejected.join(", ")
            );
        }
        if state.server != server {
            println!("  serveur : {}", state.server);
        }
        if docker {
            println!("  services : {}", visible(&services(agent, &state)));
        }
    }
    Ok(Outcome::Done)
}

fn services(agent: &Agent, state: &StackState) -> String {
    let Some(project) = running_project(agent, state) else {
        return "aucun".to_owned();
    };
    if ensure_env(&agent.paths.stack(&state.app)).is_err() {
        return "état inconnu".to_owned();
    }
    match agent.docker.ps(&project.project()) {
        Ok(statuses) if statuses.is_empty() => "aucun conteneur".to_owned(),
        Ok(statuses) => statuses
            .iter()
            .map(describe_service)
            .collect::<Vec<_>>()
            .join(", "),
        Err(error) => format!("état inconnu ({})", describe(&error)),
    }
}

fn describe_service(status: &ServiceStatus) -> String {
    let state = match status.state.as_str() {
        "running" => "en marche",
        "exited" => "arrêté",
        "restarting" => "redémarre",
        "paused" => "en pause",
        "created" => "créé",
        "dead" => "mort",
        other => other,
    };
    let health = match status.health.as_str() {
        "healthy" => " et sain",
        "unhealthy" => " mais pas sain",
        "starting" => ", démarrage en cours",
        _ => "",
    };
    format!("{} {state}{health}", status.service)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(name: &str, shared: bool) -> CreatedVolume {
        CreatedVolume {
            name: name.to_owned(),
            shared,
        }
    }

    #[test]
    fn delete_data_names_the_volumes_and_tells_the_shared_ones() {
        let notice = delete_data_notice(
            "demo",
            &[volume("demo_cache", false), volume("demo_data", false)],
        );
        assert!(
            notice.ends_with(
                "Volumes supprimés : demo_cache, demo_data, et les volumes sans nom de ses conteneurs."
            ),
            "{notice}"
        );

        let notice = delete_data_notice(
            "demo",
            &[volume("demo_data", false), volume("photos", true)],
        );
        assert!(
            notice.contains("Volumes supprimés : demo_data, photos, et"),
            "{notice}"
        );
        assert!(
            notice.ends_with(
                "Attention : le volume photos a un nom fixe. Une autre stack qui s'en sert perdrait aussi ses données."
            ),
            "{notice}"
        );

        let notice = delete_data_notice(
            "demo",
            &[volume("a", true), volume("b", true), volume("c", true)],
        );
        assert!(
            notice.contains("Attention : les volumes a, b et c ont un nom fixe."),
            "{notice}"
        );

        // Aucun volume nommé à citer (ou aucune version sur la machine) : rien n'est promis de
        // plus que la première phrase.
        let notice = delete_data_notice("demo", &[]);
        assert!(
            notice.ends_with("Rien ne pourra être récupéré."),
            "{notice}"
        );
    }
}
