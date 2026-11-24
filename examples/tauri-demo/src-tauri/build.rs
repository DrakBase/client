fn main() {
    // Les commandes de l'app sont déclarées : sans cela, toute fenêtre pourrait les appeler. Les
    // droits `allow-launch-options` et `allow-journal` sont donnés dans capabilities/default.json.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["launch_options", "journal"])),
    )
    .expect("échec de tauri-build");
}
