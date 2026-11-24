//! Clés publiques embarquées pour `self-update` : lues à la compilation dans `drakbase.json`
//! (`public_keys`, au format Tauri), que `drakbase init` écrit à la racine de la crate
//! (docs/PROTOCOLE.md, « Exécutable `bin` »). Sans ce fichier, aucune clé n'est embarquée, et
//! `self-update` refuse en l'expliquant.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let config = manifest_dir.join("drakbase.json");
    let keys = if config.is_file() {
        println!("cargo::rerun-if-changed=drakbase.json");
        read_keys(&config)
    } else {
        // Un fichier absent ferait relancer ce script à chaque compilation : on surveille le
        // dossier, dont la date change quand le fichier y est créé.
        println!("cargo::rerun-if-changed=.");
        Vec::new()
    };

    let mut code = String::from("/// Clés publiques de `drakbase.json`, au format Tauri.\n");
    code.push_str("pub const EMBEDDED_PUBLIC_KEYS: &[&str] = &[\n");
    for key in &keys {
        code.push_str(&format!("    {key:?},\n"));
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("embedded_keys.rs");
    fs::write(&out, code).expect("écriture de embedded_keys.rs");
}

fn read_keys(path: &Path) -> Vec<String> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("lecture de {} : {error}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} illisible : {error}", path.display()));
    let keys = value
        .get("public_keys")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("{} : tableau public_keys attendu", path.display()));
    keys.iter()
        .map(|key| {
            key.as_str()
                .unwrap_or_else(|| {
                    panic!(
                        "{} : public_keys ne contient que des chaînes",
                        path.display()
                    )
                })
                .trim()
                .to_owned()
        })
        .collect()
}
