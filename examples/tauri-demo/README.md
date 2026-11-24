# Démo Tauri

Petite app Tauri 2 (React et TypeScript) qui affiche sa version et se met à jour par un serveur DrakBase - Apps Market, avec le plugin officiel `tauri-plugin-updater` et l'aide [`clients/tauri`](../../clients/tauri). Elle sert d'exemple d'intégration, et à vérifier toute la chaîne : `tauri build`, `drakbase publish`, le serveur, puis la mise à jour par l'app elle-même.

Le guide d'intégration est [`clients/tauri/README.md`](../../clients/tauri/README.md) : cette page ne décrit que la démo et ses essais.

## Contenu

| Chemin | Rôle |
|---|---|
| `src/App.tsx` | interface : version, recherche, notes, installation, canal et statistiques, avec `useUpdater` |
| `src/e2e.ts`, `src-tauri/src/e2e.rs` | mode d'essai de bout en bout : options en ligne de commande et journal |
| `src-tauri/src/main.rs` | plugins (instance unique en premier, updater, process, store, os) |
| `src-tauri/tauri.conf.json` | configuration de `docs/PROTOCOLE.md`, section « Intégration » |
| `src-tauri/capabilities/default.json` | droits de la fenêtre |
| `scripts/e2e.py` | essai de bout en bout, sur macOS et sous Linux |
| `scripts/admin.py`, `scripts/relay.py` | client de l'API d'administration, et relais HTTP de l'essai |
| `linux/Dockerfile` | image de construction et d'essai sous Linux |

Le `Cargo.toml` de `src-tauri` déclare son propre `[workspace]` : l'app reste hors du workspace de la racine, avec son propre `Cargo.lock`.

## Construire

Prérequis : Rust (`/opt/homebrew/opt/rustup/bin` dans le `PATH`), Node 24 ou plus, et les outils de ligne de commande de Xcode.

```bash
npm ci
npx tauri dev
```

En développement, l'app cherche une mise à jour sur `https://maj.example.fr`, l'adresse d'exemple de `tauri.conf.json` : la recherche échoue, et l'interface le dit.

Une version publiable se construit avec la clé de signature. La démo utilise la clé d'essai de `fixtures/keys`, dont le mot de passe est public (`fixtures/README.md`) ; une vraie app crée la sienne avec `drakbase keygen`.

```bash
export TAURI_SIGNING_PRIVATE_KEY="$(cat ../../fixtures/keys/test-app.secret)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=appsmarket-fixtures
npx tauri build --bundles app
```

Pour viser un serveur local, en HTTP, sans toucher à `tauri.conf.json`, passe une configuration complémentaire :

```json
{
  "version": "0.1.1",
  "plugins": {
    "updater": {
      "endpoints": ["http://127.0.0.1:8080/api/v1/update/tauri-demo/{{target}}/{{arch}}/{{current_version}}?bundle_type={{bundle_type}}"],
      "dangerousInsecureTransportProtocol": true
    }
  }
}
```

```bash
npx tauri build --bundles app --config local.json
```

Contrôles : `npm run lint` et `npm run typecheck` ici ; `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` et `cargo test` dans `src-tauri`.

## Mode d'essai

Lancée avec `--journal <fichier>`, l'app écrit ses étapes dans ce fichier, une ligne JSON par événement : `launch` (chaque processus), `start` (version, type de bundle), `phase` (chaque étape de `useUpdater`), `before-install`, `second-instance`, `exit`. Chaque ligne porte l'heure et le PID.

| Option | Effet |
|---|---|
| `--journal <fichier>` | active le mode d'essai |
| `--auto-update` | cherche, télécharge, installe et relance sans intervention |
| `--channel stable\|beta` | enregistre le canal, comme si l'utilisateur l'avait choisi |
| `--stats on\|off` | enregistre le réglage des statistiques |
| `--quit` | envoyé par une seconde instance, en mode d'essai : ferme la première |

Les options passent par la ligne de commande, pas par l'environnement : sur macOS, Tauri relance l'app par LaunchServices (`open -n … --args`), qui transmet les arguments mais pas les variables d'environnement.

## Essai de bout en bout sur macOS

```bash
python3 scripts/e2e.py
```

Environ 5 minutes, dont 4 pour construire les quatre versions. `--work <dossier>` garde le dossier de travail, et `--reuse-builds` y reprend les builds déjà faits. Le script :
1. construit le serveur et l'outil (`cargo build --release`), puis les versions 0.1.0, 0.1.1, 0.1.2-beta.1 et 0.1.3 avec `tauri build`, signées par Tauri avec la clé d'essai ;
2. démarre `appsmarket-server` dans le dossier de travail, avec une clé maître tirée au hasard, et un relais HTTP devant lui ;
3. crée le superadmin (`create-superadmin --password-stdin`), puis, par l'API d'administration : connexion, enrôlement TOTP, mode sudo, app `tauri-demo`, clé active et jeton ;
4. se connecte avec `drakbase login --token-stdin` (`APPSMARKET_CREDENTIALS=file`, jamais le vrai trousseau), et publie avec `drakbase publish --tauri-config src-tauri`. Chaque publication doit déclarer la clé embarquée, lue dans `tauri.conf.json` ;
5. installe la 0.1.0 dans un dossier temporaire, la lance avec `open -n`, et suit le journal de l'app et celui du relais.

Les scénarios :

| Scénario | Ce qui est vérifié |
|---|---|
| Mise à jour stable | la 0.1.0 trouve, télécharge, vérifie et installe la 0.1.1, se ferme proprement (`RunEvent::Exit`), puis la 0.1.1 démarre dans un nouveau processus et se trouve à jour ; une seule instance à la fin ; adresse `…/darwin/aarch64/0.1.0?bundle_type=app` ; en-têtes du canal, de la version du système (lue par le plugin os) et de l'identifiant, le même avant et après ; téléchargement sans identifiant ni version du système |
| Instance unique | une seconde instance passe ses arguments à la première et s'arrête avant de démarrer ; `--quit` ferme l'app, sans instance restante |
| Canal bêta | la bêta n'est pas proposée sur le canal stable (204) ; sur le canal bêta, la 0.1.1 passe en 0.1.2-beta.1 ; le canal choisi est gardé d'un lancement à l'autre |
| Version retirée | la 0.1.3 est retirée pendant que l'app la télécharge : `410`, message « cette version a été retirée par l'éditeur », l'app reste en 0.1.0 ; relancée, elle reçoit la 0.1.1 |
| Réponse altérée | une réponse qui annonce 0.1.9 avec l'archive de la 0.1.1 est refusée par `requireSignedVersion` ; une signature d'une autre archive est refusée |
| Statistiques coupées | aucune vérification ne porte l'identifiant, qui est effacé du fichier de réglages |

Le script supprime `~/Library/Application Support/fr.example.tauridemo/appsmarket.json` au départ, pour partir d'une première installation. WebKit crée aussi `~/Library/WebKit/fr.example.tauridemo` et `~/Library/Caches/fr.example.tauridemo`, que l'on peut effacer après les essais.

### Résultat du 2026-10-25

Ce résultat, comme celui de Linux plus bas, date d'avant le plugin os : le contrôle de l'en-tête `X-AppsMarket-OS-Version`, ajouté depuis au scénario « Mise à jour stable », n'a pas encore été joué.

Sur un Mac Apple Silicon (macOS 27.0.1), avec `tauri` 2.12.5, `tauri-plugin-updater` 2.14.0, `tauri-plugin-single-instance` 2.5.4 et `@tauri-apps/cli` 2.12.4 : les 78 contrôles passent. Entre la réponse du serveur et la nouvelle version à jour, il s'écoule environ une demi-seconde, pour une archive de 3,5 Mo.

Ce qui est prouvé :
- le plugin Tauri lit la réponse du serveur, vérifie la signature produite par `tauri build` avec la clé d'essai, et contrôle la version signée (`requireSignedVersion`) ;
- l'archive `.app.tar.gz` de `tauri build` passe les contrôles de l'outil et du serveur (identifiant, version, arch lue dans le binaire) ;
- chaque version publiée déclare la clé qu'elle embarque, lue par l'outil dans `tauri.conf.json` ;
- la relance fonctionne avec le plugin d'instance unique, sans seconde instance qui traîne ;
- les en-têtes de l'aide arrivent au serveur, et le canal comme l'identifiant survivent à la mise à jour.

Ce qui ne l'est pas sur macOS :
- la signature par un vrai certificat (la démo est signée ad hoc) et la quarantaine d'une première installation téléchargée ;
- une app installée dans un dossier non modifiable (`/Applications` d'un autre compte), où le plugin demande le mot de passe d'un administrateur ;
- un binaire universel, et un Mac Intel ;
- les clics dans la fenêtre de l'app : les essais passent par le mode automatique, qui appelle les mêmes fonctions du crochet `useUpdater` que les boutons. L'interface a été regardée dans un navigateur, avec un faux côté Rust (`mockIPC`) : mise à jour proposée, téléchargement, installation, version retirée, canal bêta, mode sombre.

## Essai sous Linux

```bash
python3 scripts/e2e.py --linux
```

Il faut Docker (OrbStack sur le Mac) ; aucune machine virtuelle n'est créée. Environ 8 minutes, dont 5 pour construire trois AppImage dans le conteneur. Le script :
1. construit l'image `linux/Dockerfile` : Debian 12, Rust 1.99.1, Node 24 comme sur le Mac (celui de Debian est trop ancien pour Vite 8), WebKitGTK 4.1, Xvfb et D-Bus ;
2. copie les sources, sans `node_modules` ni `target`, et construit les AppImage 0.1.0, 0.1.1 et 0.1.2-beta.1 dans le conteneur, signées par Tauri avec la clé d'essai. `tauri build` télécharge linuxdeploy à la première construction ;
3. démarre le serveur et le relais sur le Mac, avec `AM_PUBLIC_URL=http://host.docker.internal:47811`, l'adresse par laquelle le conteneur joint le Mac ;
4. publie depuis le Mac avec `drakbase publish` : l'emplacement `linux/aarch64/appimage` se déduit du nom de l'AppImage ;
5. lance l'AppImage dans un conteneur qui reste allumé, avec un écran virtuel (Xvfb) et une session D-Bus pour le plugin d'instance unique. L'AppImage est rangée dans `/home/demo/Applications`, et s'extrait avant de se lancer (`APPIMAGE_EXTRACT_AND_RUN=1`), car un conteneur n'a pas FUSE.

Scénarios : mise à jour stable avec l'instance unique, canal bêta, et statistiques coupées. Le retrait et la réponse altérée ne dépendent pas du système : ils sont vérifiés sur macOS.

### Résultat du 2026-10-25

Dans un conteneur arm64 (OrbStack, Debian 12, WebKitGTK 2.50.6) : les 53 contrôles passent. Le plugin réécrit l'AppImage en place, à l'adresse de `$APPIMAGE`, puis la relance ; la seconde instance passe par D-Bus. Il s'écoule moins d'une seconde entre la recherche et la nouvelle version à jour, pour une AppImage de 91 Mo, qui embarque WebKitGTK.

Ce qui ne l'est pas sous Linux :
- un vrai bureau (GNOME, KDE) avec FUSE : ici, l'AppImage s'extrait à chaque lancement ;
- x86_64 : le conteneur est arm64, comme le Mac ;
- `deb` et `rpm`, où chaque mise à jour demande un mot de passe (pkexec) ;
- une AppImage rangée dans un dossier de root, que le plugin ne peut pas remplacer ;
- l'affichage de la fenêtre (Xvfb, sans capture).

## Windows

Rien n'est vérifiable ici, faute de PC. La liste des contrôles à faire est dans le guide, section « Plateformes et limites ».
