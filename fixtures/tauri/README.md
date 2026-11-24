# Fichiers produits par le vrai Tauri

Ils prouvent que les clés et les signatures de DrakBase - Apps Market et celles de Tauri se lisent dans les deux sens. Ils sont rejoués par `crates/am-cli/tests/tauri.rs` (l'outil face à Tauri), et par les cas de `fixtures/signatures.json` que rejouent Rust et Swift. `gen_fixtures` ne les régénère pas : ils viennent de Tauri.

Produits le 2026-10-25 avec `tauri-cli 2.12.4` (paquet npm `@tauri-apps/cli`), sur `fixtures/files/payload.bin` :

| Fichier | Origine |
|---|---|
| `tauri-generated.secret`, `tauri-generated.pub` | `tauri signer generate -w tauri.key -p tauri-compat-essai --ci` |
| `appsmarket-generated.secret`, `appsmarket-generated.pub` | `drakbase keygen compat --password-stdin`, mot de passe `appsmarket-compat-essai` (la clé de secours n'est pas gardée). Depuis, `keygen` tire les mots de passe au hasard : une clé de ce genre se fait avec `am_protocol::signing::generate_keypair` et ce mot de passe |
| `payload.bin.tauri.sig` | `tauri signer sign -f tauri.key -p tauri-compat-essai --app-version 1.2.0 payload.bin` |
| `payload.bin.tauri-no-version.sig` | la même commande, sans `--app-version` |
| `payload.bin.tauri-with-appsmarket-key.sig` | `tauri signer sign -f compat.key -p appsmarket-compat-essai --app-version 1.2.0 payload.bin` |

Les fichiers de clés secrètes portent l'extension `.secret`, comme dans `fixtures/keys`, car `.gitignore` écarte les `*.key`.

**Clés d'essai uniquement.** Leurs mots de passe sont publics : ne jamais les utiliser pour une vraie app.

Pour les refaire, installer le paquet npm dans un dossier temporaire (`npm install --prefix <dossier> --ignore-scripts @tauri-apps/cli@2.12.4`), puis lancer les commandes ci-dessus avec `node <dossier>/node_modules/@tauri-apps/cli/tauri.js`.
