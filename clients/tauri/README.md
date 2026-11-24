# Intégrer DrakBase - Apps Market dans une app Tauri 2

Ce guide branche une app Tauri 2 (macOS, Linux, Windows) sur un serveur DrakBase - Apps Market. L'app utilise le plugin officiel `tauri-plugin-updater` en mode « serveur dynamique » : elle demande au serveur s'il existe une version plus récente, télécharge l'archive, en vérifie la signature avec la clé publique qu'elle embarque, l'installe, puis se relance. Le contrat est dans [`docs/PROTOCOLE.md`](../../docs/PROTOCOLE.md), les choix dans [`docs/PLAN.md`](../../docs/PLAN.md).

Ce dossier contient :
- ce guide ;
- `@drakbase/tauri`, une petite aide TypeScript : `checkForUpdate`, `installUpdate` et le crochet React `useUpdater`. Elle ajoute les en-têtes du canal, de la version du système et de l'identifiant d'installation, et range les réglages dans les données de l'app.

L'app d'exemple [`examples/tauri-demo`](../../examples/tauri-demo) applique tout ce guide, et son essai de bout en bout le vérifie.

## En bref : `drakbase init`

À la racine de l'app, `drakbase init mon-app` fait les sections 1 et 2, la section 3 sauf le code Rust, et l'installation de la section 4 :
- il crée les clés si aucune n'existe, et les sauvegarde dans KeePassXC ;
- il complète `tauri.conf.json`, `capabilities/default.json`, `Cargo.toml` et `package.json`, sans toucher au code ni remplacer une clé déjà embarquée ;
- il déclare l'app et sa clé sur le serveur : l'administration s'ouvre sur une page pré-remplie, à valider ;
- il affiche ce qui reste à faire (enregistrer les plugins, l'interface, la publication), puis les contrôles de `drakbase doctor` qui ne passent pas encore.

`drakbase doctor` refait ces contrôles à tout moment, sans rien modifier, serveur compris. Le détail de chaque étape suit.

## Versions

| Paquet | Version | Pourquoi |
|---|---|---|
| `tauri`, `@tauri-apps/api` | 2.12 | |
| `@tauri-apps/cli` | 2.12 ou plus | `tauri build` écrit `version:` dans le commentaire de confiance de la signature |
| `tauri-plugin-updater`, `@tauri-apps/plugin-updater` | 2.14 ou plus | `requireSignedVersion` n'existe qu'à partir de 2.14 |
| `tauri-plugin-process`, `@tauri-apps/plugin-process` | 2.4 | `relaunch()` après l'installation |
| `tauri-plugin-store`, `@tauri-apps/plugin-store` | 2.5 | réglages de l'aide (`appsmarket.json`) |
| `tauri-plugin-os`, `@tauri-apps/plugin-os` | 2.4 | version du système envoyée au serveur, voir « La version du système » |
| `tauri-plugin-single-instance` | 2.5, facultatif | compatible avec la relance, voir « Instance unique » |

Épingle les crates et les paquets npm sur des versions exactes, les mêmes des deux côtés : `tauri build` refuse un paquet npm d'une autre version mineure que sa crate. L'exemple utilise `tauri` 2.12.5, `tauri-plugin-updater` 2.14.0, `tauri-plugin-process` 2.4.0, `tauri-plugin-store` 2.5.0, `tauri-plugin-os` 2.4.0 et `tauri-plugin-single-instance` 2.5.4, avec `@tauri-apps/cli` et `@tauri-apps/api` 2.12.4.

## 1. Les clés de signature

Une paire de clés par app : avec une clé commune, un serveur compromis pourrait servir une app aux utilisateurs d'une autre.

`drakbase init mon-app` les crée s'il n'y en a pas, comme `drakbase keygen mon-app`. Personne ne choisit ni ne tape leurs mots de passe : l'outil les tire au hasard, puis :
1. il sauvegarde les deux clés et leurs mots de passe dans ta base KeePassXC, dans le groupe « Drakbase - Apps Market » ; il demande pour cela le mot de passe de la base, et retient la base pour les fois suivantes ;
2. il relit les deux entrées, et vérifie que chaque clé s'ouvre avec le mot de passe relu ;
3. il écrit, dans `~/.appsmarket/keys/` :
   - `mon-app.key` : la clé privée, chiffrée, au format de Tauri. Elle ne va jamais sur le serveur ni dans un dépôt. Son mot de passe va dans le trousseau macOS ;
   - `mon-app.key.pub` : la clé publique, à mettre dans `tauri.conf.json` et sur le serveur ;
   - `mon-app.backup.key.pub` : la clé publique de secours. La clé secrète de secours ne reste que dans KeePassXC. Le plugin Tauri n'accepte qu'une clé publique, il ne s'en sert donc pas.

Si la clé privée est perdue, plus aucune mise à jour ne passera : il faudra que chaque utilisateur réinstalle l'app depuis la page publique. Ta base KeePassXC doit donc être sauvegardée hors de ce Mac. Sur un nouveau Mac, `drakbase restore mon-app` réinstalle les clés depuis KeePassXC.

**Changer de clé** demande une version de transition : signée avec l'ancienne clé, elle embarque déjà la nouvelle dans `pubkey`. Pour que le serveur la propose aux anciennes installations, au lieu d'une version signée avec la nouvelle clé qu'elles refuseraient, chaque version doit déclarer la clé qu'elle embarque (`trusted_key_ids`, docs/API.md).

`drakbase publish` la déclare lui-même, dès la première version (section 5). Il lit `plugins.updater.pubkey` dans la configuration de l'app :
- `src-tauri/tauri.conf.json`, ou `tauri.conf.json`, dans le dossier courant ;
- ou la configuration donnée par `--tauri-config` (le fichier, ou son dossier).

Un fichier propre à une plateforme (`tauri.macos.conf.json`, `tauri.windows.conf.json`, `tauri.linux.conf.json`) qui remplace la clé l'emporte pour les artefacts de cette plateforme.

Sans configuration trouvée, l'outil refuse `--publish`. Sans clé déclarée, le serveur proposerait aux installations de cette version n'importe quelle version plus récente, quelle que soit sa clé : après une rotation, elles recevraient une version qu'elles refusent, et resteraient bloquées. Une version publiée ne se corrige plus.

Si la clé embarquée ne vient pas de ces fichiers, par exemple passée au build par `tauri build --config`, procède ainsi :
1. publie sans `--publish` : la version reste en brouillon ;
2. dans l'administration, coche la clé dans « Clés embarquées » du brouillon ;
3. publie depuis l'administration.

L'app d'exemple utilise la clé d'essai `fixtures/keys/test-app.secret`, dont le mot de passe est public (`fixtures/README.md`). Une vraie app crée la sienne avec `drakbase keygen`.

## 2. L'app sur le serveur

`drakbase init mon-app`, connecté au serveur par `drakbase login`, ouvre l'administration sur une page pré-remplie : le slug (`mon-app`, il ne change plus), le type `tauri`, le nom, l'identifiant de bundle (`identifier` de `tauri.conf.json`, que le serveur compare à celui des archives macOS) et l'identifiant de la clé, le même qu'au terminal. Un clic crée l'app et active sa clé, avec le mode sudo et le TOTP du superadmin. L'outil attend que le serveur la montre, puis continue.

Le jeton de l'outil ne peut ni créer d'app ni déclarer de clé : un jeton volé ne permet donc toujours pas de livrer du code.

À la main, dans l'administration (superadmin) :
1. **Apps → Nouvelle app** : le slug, le type `tauri` et l'identifiant de bundle.
2. **Clés** : ajoute le contenu de `mon-app.key.pub` comme clé active.
3. **Fiche de l'app**, conseillé : les **types par défaut**, `nsis` pour Windows et `appimage` pour Linux. Ils servent aux clients qui n'envoient pas leur type de bundle.
4. **Mon compte → Jetons** : un jeton `releases:write` et `releases:publish`, limité à l'app, pour l'outil.

Les mêmes actions existent dans l'API d'administration (`docs/API.md`).

## 3. La configuration de l'app

**`src-tauri/tauri.conf.json`** :

```json
{
  "identifier": "fr.example.mon-app",
  "bundle": {
    "targets": ["app", "dmg", "nsis", "appimage"],
    "createUpdaterArtifacts": true,
    "macOS": { "signingIdentity": "<nom de ton certificat>" },
    "windows": { "nsis": { "installMode": "currentUser" } }
  },
  "plugins": {
    "updater": {
      "pubkey": "<contenu de mon-app.key.pub>",
      "endpoints": [
        "https://updates.drakbase.com/api/v1/update/mon-app/{{target}}/{{arch}}/{{current_version}}?bundle_type={{bundle_type}}"
      ],
      "requireSignedVersion": true,
      "windows": { "installMode": "passive" }
    }
  }
}
```

- `{{bundle_type}}` dit au serveur comment l'app a été installée (`app`, `nsis`, `appimage`, `deb`…), pour qu'il serve le même type. Sans lui, une installation `deb` pourrait recevoir une AppImage.
- `requireSignedVersion` refuse une archive dont la version signée n'est pas celle annoncée : un serveur compromis ne peut pas présenter une ancienne version, correctement signée, sous un numéro plus haut.
- `createUpdaterArtifacts: true` produit les archives de mise à jour et leurs signatures. N'utilise pas `"v1Compatible"`.
- `bundle.targets` est filtré par plateforme : la même liste convient sur les trois systèmes. Le DMG sert à la première installation, jamais aux mises à jour. MSI est évité : il s'installe pour toute la machine et demande l'UAC à chaque mise à jour.
- `installMode: currentUser` installe l'app sans droits d'administrateur, et `windows.installMode: passive` fait les mises à jour avec une simple barre de progression.
- L'adresse doit être en `https`. `dangerousInsecureTransportProtocol` ne sert qu'aux essais en local.
- `signingIdentity` : le nom du certificat auto-signé (PLAN.md, « Clés et certificat »). L'exemple met `"-"`, une signature ad hoc, suffisante pour l'essai : le plugin ne contrôle pas la signature de code.

**`src-tauri/Cargo.toml`** :

```toml
[dependencies]
tauri = { version = "=2.12.5", features = [] }
tauri-plugin-os = "=2.4.0"
tauri-plugin-process = "=2.4.0"
tauri-plugin-single-instance = "=2.5.4"   # facultatif
tauri-plugin-store = "=2.5.0"
tauri-plugin-updater = "=2.14.0"
```

**`src-tauri/src/main.rs`** (ou `lib.rs`) :

```rust
fn main() {
    tauri::Builder::default()
        // Toujours en premier : une seconde instance doit s'arrêter avant le reste.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_os::init())
        .run(tauri::generate_context!())
        .expect("impossible de démarrer l'app");
}
```

**`src-tauri/capabilities/default.json`** : la fenêtre qui gère les mises à jour a besoin de `updater:default`, `process:allow-restart` et `store:default`. Le plugin os n'en demande aucune : il donne la version du système à la page dès son chargement.

## 4. L'interface : `@drakbase/tauri`

### Installation

L'aide n'est pas publiée sur npm : elle s'installe depuis le dépôt public des clients de DrakBase, par HTTPS. Les versions suivent les étiquettes du dépôt (`v0.6.0`…). Jusqu'à la 0.2, le paquet s'appelait `@appsmarket/tauri`. npm n'en copie que le `package.json` de la racine, les sources de l'aide, ce guide et le `README.md` du dépôt, que npm ajoute toujours. Ses imports (`@tauri-apps/…`, `react`) se résolvent dans les dépendances de l'app, en un seul exemplaire.

```json
{
  "dependencies": {
    "@drakbase/tauri": "git+https://github.com/DrakBase/client.git#semver:^0.6.0",
    "@tauri-apps/api": "2.12.4",
    "@tauri-apps/plugin-os": "2.4.0",
    "@tauri-apps/plugin-process": "2.4.0",
    "@tauri-apps/plugin-store": "2.5.0",
    "@tauri-apps/plugin-updater": "2.14.0"
  }
}
```

**Passer à une aide plus récente.** En 0.x, `^` ne franchit pas la version mineure : `#semver:^0.4.0` ne reçoit jamais la 0.5, ni `^0.5.3` la 0.6. `npm update` ne suffit donc pas :
1. relance `drakbase init mon-app` avec l'outil à jour : il monte la plage de `@drakbase/tauri` à sa propre version, et ajoute les plugins que cette version demande (le plugin os, à partir de la 0.5.1) ;
2. lance `npm install` ;
3. enregistre côté Rust les plugins ajoutés (section 3).

`drakbase doctor` signale une aide plus ancienne que l'outil : sans cela, une app restée en 0.4 aurait le plugin os installé et enregistré, sans que rien n'envoie la version du système.

L'aide est livrée en TypeScript, compilé par l'app : elle suppose `"moduleResolution": "bundler"`, ce que font Vite et les modèles de Tauri. Elle n'impose aucune version de Node à l'app : `engines` (Node 24 ou plus) n'est déclaré que dans le `package.json` de ce dossier, pour ses propres tests.

Pour essayer une modification de l'aide avant de l'étiqueter, une app voisine peut la prendre par son chemin, comme l'app d'exemple : `"file:../DrakBase - Apps Market/clients/tauri"`, avec `install-links=true` dans un `.npmrc` à côté de `package.json`, pour que npm la copie dans `node_modules` au lieu d'un lien. Après chaque modification, relance `npm ci` : `npm install` garde l'ancienne copie. Quand la version de l'aide ou ses dépendances changent, `npm install "@drakbase/tauri@file:<chemin>" --package-lock-only` met à jour sa ligne dans `package-lock.json`, que `npm install` seul laisse telle quelle.

### Le crochet React

```tsx
import { useUpdater } from "@drakbase/tauri/react";

function Updates() {
  const updater = useUpdater(); // cherche une mise à jour au montage
  const { phase, update, progress, error, settings } = updater;

  return (
    <section>
      {update && (phase === "available" || phase === "error") ? (
        <>
          <p>La version {update.version} est disponible.</p>
          <pre>{update.notes}</pre>
          <button onClick={() => void updater.install()}>Installer et relancer</button>
        </>
      ) : null}
      {phase === "downloading" && progress ? (
        <progress max={progress.total ?? undefined} value={progress.downloaded} />
      ) : null}
      {phase === "error" ? <p>{error}</p> : null}
      <button onClick={() => void updater.check()}>Rechercher les mises à jour</button>
      <select
        value={settings?.channel ?? "stable"}
        onChange={async (event) => {
          await updater.setChannel(event.target.value === "beta" ? "beta" : "stable");
          await updater.check();
        }}
      >
        <option value="stable">Stable</option>
        <option value="beta">Bêta</option>
      </select>
      <label>
        <input
          type="checkbox"
          checked={settings?.shareStatistics ?? true}
          onChange={(event) => void updater.setShareStatistics(event.target.checked)}
        />
        Envoyer des statistiques anonymes
      </label>
    </section>
  );
}
```

- **Phases** : `idle`, `checking`, `upToDate`, `available`, `downloading`, `installing`, `error`.
- **État** : `currentVersion`, `settings` (`channel`, `shareStatistics`), `update` (`version`, `notes` en Markdown, `date`, `channel`), `progress` (`downloaded`, `total`), `error` (message en français), `lastCheck` (la dernière recherche réussie : une recherche qui échoue ne la change pas).
- **Options** : `checkOnMount` (vrai par défaut), `checkInterval`, `osVersion`, `timeout`, `downloadTimeout`, `beforeInstall`.
- **Recherche périodique** : après celle du montage, le crochet cherche de nouveau à intervalle, pour qu'une app laissée ouverte voie les nouvelles versions.
  - `checkInterval` est en millisecondes : un jour par défaut, ramené entre une heure et trente jours, comme dans le paquet Swift. `0` ou `null` : l'app ne cherche qu'au montage et à la demande.
  - L'intervalle court depuis la dernière recherche, demandée ou non. Si l'horloge recule, l'attente repart pour un intervalle au plus.
- **Recherche qui échoue** : une version déjà proposée n'est pas oubliée, elle reste à installer (`update`).
  - Automatique, la recherche se tait : la phase revient à `available`.
  - Demandée, elle dit son erreur : la phase passe à `error`, et `update` garde la version. Une interface qui veut encore proposer l'installation teste donc `update`, pas seulement la phase `available`.
- **Délais** : passé son délai, le plugin abandonne la requête, la phase passe à `error`, et le crochet est de nouveau libre.
  - `timeout`, pour la recherche : 60 000 ms par défaut, comme dans le paquet Swift.
  - `downloadTimeout`, pour le téléchargement, du premier au dernier octet : 4 heures par défaut, comme dans le paquet Swift. Sans délai, un serveur qui cesse d'envoyer laisserait le crochet occupé jusqu'à la fermeture de l'app.
- `osVersion` : la version du système, pour que le serveur écarte les versions qui en exigent un plus récent. L'aide la lit seule ; l'option ne sert qu'à en imposer une autre (voir « La version du système »).
- `beforeInstall` est appelé quand l'archive est téléchargée et vérifiée, juste avant l'installation : c'est le moment d'enregistrer un travail en cours. Sous Windows, l'installeur ferme l'app aussitôt.
- **Changer de canal** oublie la mise à jour affichée : cherche à nouveau ensuite. Pendant une recherche, `setChannel` enregistre le canal, et la recherche se refait seule avec lui : sa première réponse valait pour l'ancien. Pendant une installation, celle-ci va à son terme.

### Sans React

```ts
import {
  checkForUpdate,
  describeUpdateError,
  installUpdate,
  isWithdrawnUpdateError,
  updateChannel,
} from "@drakbase/tauri";

let update = await checkForUpdate();
if (update) {
  console.log(update.version, updateChannel(update)); // « stable » ou « beta »
  try {
    await installUpdate(update, {
      onProgress: ({ downloaded, total }) => console.log(downloaded, total),
      beforeInstall: () => saveDraft(),
    });
  } catch (error) {
    showError(describeUpdateError(error)); // message en français
    if (isWithdrawnUpdateError(error)) {
      // Version retirée par l'éditeur (410) : elle ne reviendra pas, on cherche à nouveau.
      await update.close();
      update = await checkForUpdate();
    }
  }
}
```

- `checkForUpdate` renvoie l'objet `Update` du plugin, ou `null` si l'app est à jour. Ses options `channel` et `shareStatistics` l'emportent sur les réglages enregistrés ; `osVersion` impose une version du système à la place de celle que l'aide lit, et `timeout` le délai de la recherche (60 000 ms par défaut). Sans le crochet, c'est à l'app de chercher de nouveau de temps en temps.
  - Elle rend `null` aussi pour une bêta reçue sur le canal stable, et pour une version qui exige un système plus récent que celui de la machine (`min_os_version`).
  - Elle lève une erreur si la réponse n'annonce pas un canal `stable` ou `beta`.
- `updateChannel(update)` rend le canal de la version proposée, `stable` ou `beta`, tel que l'annonce le serveur.
- `installUpdate` télécharge, fait vérifier la signature par le plugin, installe, puis appelle `relaunch()` : sur macOS et Linux, le plugin ne relance pas l'app lui-même. Son option `downloadTimeout` borne le téléchargement (4 heures par défaut).
- `describeUpdateError(error)` rend un message en français pour une erreur du plugin, dont les messages sont en anglais : serveur injoignable, signature refusée, version signée différente, fichier introuvable, version retirée. Le texte d'origine reste à la fin, entre parenthèses.
- `isWithdrawnUpdateError(error)` est vrai quand le téléchargement a reçu `410` : l'éditeur a retiré la version entre la recherche et le téléchargement. Réessayer ne sert à rien : libère la mise à jour (`update.close()`), puis cherche à nouveau. Le crochet, lui, libère et oublie la version tout seul ; il reste à relancer `check()`.
- `loadSettings`, `saveSettings` et `installId` donnent accès aux réglages.

### Ce que l'aide envoie

- `X-AppsMarket-Channel` : `stable` ou `beta`, le réglage de l'utilisateur.
- `X-AppsMarket-Install-Id` : seulement si les statistiques sont activées. C'est un UUID v4 tiré au hasard à la première recherche, jamais dérivé du matériel.
- `X-AppsMarket-OS-Version` : la version du système, lue par le plugin os (section suivante). Le serveur écarte alors les versions qui exigent un OS plus récent.

Le téléchargement part sans ces en-têtes.

### Ce que l'aide contrôle

L'aide ne se fie pas au seul serveur, comme le paquet Swift (`docs/PROTOCOLE.md`, « Intégration », qui liste aussi ce qui diffère entre les deux clients) :
- une réponse dont le `channel` n'est ni `stable` ni `beta` est refusée : la recherche échoue ;
- une bêta reçue alors que l'app suit le canal stable est traitée comme « à jour » ;
- une version dont le `min_os_version` dépasse la version du système lue est traitée comme « à jour ». Les deux se comparent comme côté serveur (`docs/PROTOCOLE.md`, « Versions d'OS ») : les tests de l'aide rejouent `fixtures/os-versions.json`, comme ceux de Rust et de Swift. Sans version du système (plugin os absent, ou `osVersion: ""`), rien n'est écarté.

La signature, la version signée et le refus de revenir en arrière restent contrôlés par le plugin.

### La version du système (`osVersion`)

L'aide envoie la version du système à chaque recherche, pour que le serveur écarte une version trop récente pour la machine. Sans elle, un Mac resté sur un ancien macOS recevrait une version qui exige un macOS plus récent (`bundle.macOS.minimumSystemVersion`), le plugin l'installerait, et l'app ne se lancerait plus. Le paquet Swift l'envoie aussi.

La webview ne donne pas cette version de façon fiable : l'aide la lit par le plugin officiel `os`, que `drakbase init` ajoute avec les autres.

1. Le plugin est à la même version des deux côtés (2.4.0 au moment d'écrire ces lignes) :

   ```toml
   tauri-plugin-os = "=2.4.0"
   ```

   ```json
   "@tauri-apps/plugin-os": "2.4.0"
   ```

2. Il s'enregistre avec les autres : `.plugin(tauri_plugin_os::init())`.
3. Aucune permission n'est à ajouter : le plugin donne la version à la page dès son chargement. Sur macOS, c'est `14.6.1`.

Si le plugin n'est pas enregistré côté Rust, la recherche part sans cette version, comme avant la 0.5 : `drakbase doctor` le signale. L'aide ne peut alors pas non plus écarter elle-même une version trop récente.

L'option `osVersion` impose une autre version : `useUpdater({ osVersion: "13.5" })`, ou `checkForUpdate({ osVersion: "13.5" })` sans React. Une chaîne vide n'envoie rien, et ne contrôle rien.

### Où sont rangés les réglages

Dans `appsmarket.json`, le fichier du plugin store, dans le dossier de données de l'app :
- macOS : `~/Library/Application Support/<identifier>/appsmarket.json` ;
- Linux : `~/.local/share/<identifier>/appsmarket.json` ;
- Windows : `%APPDATA%\<identifier>\appsmarket.json`.

Clés : `channel`, `shareStatistics`, `installId`. Couper les statistiques efface l'identifiant ; si l'utilisateur les réactive, il en reçoit un nouveau, sans lien avec l'ancien. Les statistiques sont activées par défaut, comme dans le paquet Swift `DrakBase` : dis-le dans l'interface, à côté de la case.

## 5. Construire et publier

**Construire**, avec la clé de l'app et son mot de passe, pris dans le trousseau :

```bash
drakbase exec mon-app -- npx tauri build
```

`drakbase exec` vérifie que le mot de passe ouvre la clé, puis lance la commande avec `TAURI_SIGNING_PRIVATE_KEY` (le chemin de `mon-app.key`) et `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Ces deux variables, définies à la main, font la même chose.

`tauri build` signe chaque archive de mise à jour et écrit sa `.sig` à côté, avec `version:<version>` dans le commentaire de confiance. La version vient de `tauri.conf.json` : écris-la sous sa forme canonique (`1.2.0`, `1.3.0-beta.1`), car le plugin lit les versions strictement.

| Système | Archive de mise à jour | Signature | Première installation |
|---|---|---|---|
| macOS | `bundle/macos/<Nom>.app.tar.gz` | `.app.tar.gz.sig` | `bundle/dmg/<Nom>_<version>_<arch>.dmg` |
| Linux | `bundle/appimage/<nom>_<version>_<arch>.AppImage` | `.AppImage.sig` | la même AppImage |
| Windows | `bundle/nsis/<Nom>_<version>_<arch>-setup.exe` | `-setup.exe.sig` | le même installeur |

Pour un binaire macOS universel : `npx tauri build --target universal-apple-darwin`.

**Publier**, une fois connecté (`drakbase login`) :

```bash
drakbase publish --app mon-app --version 1.2.0 --channel stable --notes notes.md --publish \
  "src-tauri/target/release/bundle/macos/Mon App.app.tar.gz" \
  darwin/aarch64/dmg=src-tauri/target/release/bundle/dmg/Mon\ App_1.2.0_aarch64.dmg \
  chemin/vers/mon-app_1.2.0_amd64.AppImage \
  chemin/vers/Mon\ App_1.2.0_x64-setup.exe
```

- L'outil lit chaque `.sig` à côté de son fichier, vérifie la signature avec la clé de l'app, et refuse une version inférieure ou égale à la dernière du canal (sauf `--force`).
- Il déclare la clé que la version embarque, lue dans `src-tauri/tauri.conf.json` : lance la commande depuis le dossier du projet, ou donne la configuration avec `--tauri-config` (section 1).
- L'emplacement (`os/arch/type`) se déduit du nom : `.app.tar.gz` (arch lue dans le binaire), `.dmg`, `.AppImage`, `.deb`, `.rpm`, `-setup.exe` (NSIS), `.msi`, avec l'arch écrite par Tauri (`aarch64`, `amd64`, `x64`…). Sinon, donne-le : `darwin/aarch64/dmg=…`.
- Sans `--publish`, la version reste en brouillon ; elle se publie ensuite dans l'administration. Une publication interrompue reprend avec la même commande.
- L'outil ne compile que sur macOS : les builds Linux et Windows reviennent sur le Mac pour être publiés.

## 6. Canaux, retrait et statistiques

- **Canal bêta** : une app réglée sur bêta voit les versions stables et bêta, donc toujours la plus haute. Publie une bêta avec `--channel beta` et une version de pré-publication (`1.3.0-beta.1`).
- **Passer une bêta en stable** : dans l'administration, change le canal de la version. Le canal ne fait pas partie de la signature.
- **Retirer une version** : dans l'administration, ou par l'API avec le jeton. Elle sort des propositions, et ses téléchargements répondent `410`. Un client qui la téléchargeait affiche « cette version a été retirée par l'éditeur ; recherche à nouveau ». On ne revient jamais en arrière : on corrige avec une version plus haute, et le plugin n'installe qu'une version strictement plus récente.
- **Statistiques** : le serveur ne garde qu'une empreinte du jour de l'identifiant, effacée chaque nuit après agrégation, et aucune adresse IP (PROTOCOLE.md, « Statistiques »).

## 7. Plateformes et limites

### macOS

- Mise à jour : `.app.tar.gz`, que le plugin extrait puis échange avec l'app en place (`renamex_np`, atomique sur APFS). Il faut pouvoir écrire dans le dossier de l'app ; sinon, macOS demande le mot de passe d'un administrateur.
- Relance : Tauri relance l'app par LaunchServices (`open -n`). La nouvelle instance reçoit les mêmes arguments, mais pas les variables d'environnement du processus qui l'a lancée.
- Premier lancement : sans notarisation, Gatekeeper bloque l'app téléchargée. La page publique explique « Ouvrir quand même ». Une app lancée depuis le DMG ou le dossier Téléchargements, encore en quarantaine, tourne depuis une copie temporaire en lecture seule (App Translocation) : demande de la glisser dans Applications avant la première mise à jour.
- Les mises à jour, téléchargées par le plugin et non par un navigateur, ne sont pas mises en quarantaine.
- `bundle.macOS.minimumSystemVersion` devient `LSMinimumSystemVersion`, que le serveur relève à l'envoi. L'aide envoie la version du système, pour qu'il écarte les versions trop récentes pour le Mac (section 4, « La version du système »).

### Linux

- Mise à jour en place : seulement l'AppImage. Le plugin réécrit le fichier désigné par `$APPIMAGE`, puis la relance : il doit se trouver dans un dossier de l'utilisateur, pas dans un dossier de root.
- L'AppImage de Tauri embarque WebKitGTK : compte environ 90 Mo par version et par arch, contre 3,5 Mo pour l'archive macOS.
- Le plugin d'instance unique passe par la session D-Bus de l'utilisateur.
- Certaines distributions n'ont pas FUSE 2, dont l'AppImage a besoin. Sans lui, elle se lance avec `--appimage-extract-and-run`, ou avec la variable `APPIMAGE_EXTRACT_AND_RUN=1`.
- `deb` et `rpm` sont servis seulement aux installations de ce type. Chaque mise à jour demande alors un mot de passe (pkexec, sinon sudo).

### Windows (à vérifier sur un PC)

Rien n'a pu être essayé ici : il n'y a pas de PC Windows. Ce qu'on attend :
- l'installeur NSIS en `currentUser` installe l'app dans `%LOCALAPPDATA%\<productName>`, sans UAC ;
- à la mise à jour, le plugin lance l'installeur en mode passif (`/P`), avec `/R` pour relancer l'app, puis ferme l'app : `downloadAndInstall` et `installUpdate` ne reviennent pas ;
- SmartScreen avertit à la première installation d'un installeur non signé ; en mode strict, Smart App Control le bloque, et seule une vraie signature de code règle ce cas. Les mises à jour, elles, passent.

Liste de contrôle pour le premier essai sur un PC :
1. Construire la démo sous Windows (`npx tauri build`, avec la clé d'essai), puis publier `…_x64-setup.exe` et sa `.sig` depuis le Mac, dans `windows/x86_64/nsis`.
2. Installer la 0.1.0 avec l'installeur : aucune demande d'élévation, et l'app dans `%LOCALAPPDATA%\Tauri Demo`.
3. Publier la 0.1.1, puis « Rechercher les mises à jour » : la vérification porte `?bundle_type=nsis` et les en-têtes `X-AppsMarket-*`.
4. « Installer et relancer » : une fenêtre de progression, sans question ni UAC, puis la 0.1.1 qui démarre seule.
5. Une seule instance à la fin (Gestionnaire des tâches), y compris avec le plugin d'instance unique, et `beforeInstall` appelé avant la fermeture.
6. `%APPDATA%\fr.example.tauridemo\appsmarket.json` contient l'identifiant ; le décocher l'efface.
7. Le canal bêta, puis une version retirée (`410`, message en français).
8. Une app installée par MSI ne reçoit jamais l'installeur NSIS (`?bundle_type=msi`).
9. SmartScreen à la première installation, et le comportement de Smart App Control s'il est actif.

## 8. Instance unique

Le risque noté dans PLAN.md, une relance bloquée par le plugin d'instance unique, ne se produit pas avec `tauri` 2.12.5 et `tauri-plugin-single-instance` 2.5.4, sur macOS comme sous Linux (AppImage) : les essais de bout en bout de la démo le vérifient. `relaunch()` passe par `RunEvent::Exit`, où le plugin libère sa socket sur macOS (`/tmp/<identifier>_si.sock`) et son nom D-Bus sous Linux, avant que Tauri ne lance la nouvelle instance.

Deux conditions :
- le plugin d'instance unique est enregistré en premier ;
- l'app ne bloque pas la fermeture. Si elle appelle `api.prevent_exit()` sur `RunEvent::ExitRequested`, par exemple pour rester dans la barre des menus, elle doit laisser passer le code de relance :

```rust
.run(|_app, event| {
    if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
        if code != Some(tauri::RESTART_EXIT_CODE) {
            api.prevent_exit();
        }
    }
})
```

Sous Windows, le plugin passe par un mutex nommé : la relance par l'installeur NSIS reste à vérifier sur un PC.

## 9. Sécurité, en résumé

- La clé privée reste sur le poste du développeur ; le serveur vérifie les signatures mais ne signe jamais.
- L'app vérifie elle-même la signature (minisign « ED »), la clé propre à l'app et la version signée (`requireSignedVersion`). Elle refuse de revenir en arrière.
- Un serveur ou un jeton compromis peut retenir des mises à jour, pas en livrer une fausse.
- HTTPS obligatoire en production.
