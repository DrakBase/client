# DrakBase - client

Ce qu'il faut pour brancher une app sur [DrakBase](https://updates.drakbase.com), un serveur de mises à jour pour des apps macOS en Swift, des apps Tauri 2 (macOS, Linux, Windows), des stacks Docker et des exécutables Linux :

| Partie | Dossier |
|---|---|
| le paquet Swift `DrakBase`, que l'app importe | `clients/swift/DrakBase` |
| l'aide `@drakbase/tauri`, avec le plugin officiel `tauri-plugin-updater` | `clients/tauri` |
| l'outil `drakbase`, sur le Mac du développeur : clés, branchement de l'app, signature, publication | `crates/am-cli` |
| l'agent `drakbase-stack`, qui installe et met à jour les stacks Docker sur une machine Linux | `crates/am-agent` |
| le protocole partagé, ses jeux d'essai et les contrats avec le serveur | `crates/am-protocol`, `fixtures`, `docs` |

Chaque version est signée sur le Mac du développeur, avec une clé propre à l'app. L'app vérifie elle-même la signature, la version signée et son identifiant : un serveur compromis ne pourrait que retenir des mises à jour, jamais en livrer une fausse.

## Installer

L'outil, sur un Mac avec Rust :

```bash
cargo install --git https://github.com/DrakBase/client --tag v0.6.0 am-cli --locked
```

Le paquet Swift, dans `Package.swift`, ou dans Xcode par File › Add Package Dependencies… :

```swift
.package(url: "https://github.com/DrakBase/client.git", from: "0.6.0"),
// dans la cible de l'app :
.product(name: "DrakBase", package: "client"),
```

L'aide Tauri, dans `package.json` :

```json
"@drakbase/tauri": "git+https://github.com/DrakBase/client.git#semver:^0.6.0"
```

`drakbase init <slug>`, lancé à la racine du projet, écrit lui-même la configuration et ces dépendances, puis dit ce qui reste à écrire. Les guides complets sont dans `clients/swift/DrakBase/README.md` et `clients/tauri/README.md`.

Avec Claude Code, la skill `drakbase` fait ce parcours avec vous :

```bash
claude plugin marketplace add DrakBase/skill
claude plugin install drakbase@drakbase
```

## Ce dépôt

Il est publié à chaque version depuis le dépôt où DrakBase est développé, avec le serveur : un commit par version, sous l'étiquette `vX.Y.Z`, la même pour toutes les parties. Les renvois à `docs/PLAN.md` dans le code désignent la conception, tenue avec le serveur.

Les essais se lancent ici aussi :

```bash
cargo test --workspace
swift test --package-path clients/swift/DrakBase
cd clients/tauri && npm ci && npm test
```

## Licence

MIT, voir `LICENSE`.
