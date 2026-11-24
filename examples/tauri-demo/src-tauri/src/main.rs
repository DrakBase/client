//! Démo Tauri de DrakBase - Apps Market : une fenêtre qui affiche sa version et se met à jour avec
//! `tauri-plugin-updater`, branché sur un serveur DrakBase - Apps Market (docs/PROTOCOLE.md, « Intégration »).
//! La recherche et l'installation se pilotent depuis l'interface, avec l'aide de `clients/tauri`.

// Pas de fenêtre de console sous Windows, hors développement.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod e2e;

use serde_json::json;
use tauri::Manager as _;

use crate::e2e::LaunchOptions;

fn main() {
    let options = LaunchOptions::from_args(std::env::args());
    options.journal(
        "launch",
        json!({ "args": std::env::args().skip(1).collect::<Vec<_>>() }),
    );

    let app = tauri::Builder::default()
        // En premier : une seconde instance doit s'arrêter avant que le reste démarre. Elle
        // transmet ses arguments à la première, puis quitte.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let options = app.state::<LaunchOptions>();
            options.journal("second-instance", json!({ "args": args }));
            if options.is_e2e() && args.iter().any(|arg| arg == "--quit") {
                app.exit(0);
                return;
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_os::init())
        .manage(options)
        .invoke_handler(tauri::generate_handler![e2e::launch_options, e2e::journal])
        .setup(|app| {
            let version = app.package_info().version.to_string();
            let bundle_type = tauri::utils::platform::bundle_type().map(|kind| format!("{kind:?}"));
            app.state::<LaunchOptions>().journal(
                "start",
                json!({ "version": version, "bundleType": bundle_type }),
            );
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("impossible de démarrer l'app");

    app.run(|app, event| {
        if let tauri::RunEvent::Exit = event {
            app.state::<LaunchOptions>().journal("exit", json!({}));
        }
    });
}
