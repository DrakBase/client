# Protocole de mise à jour

Contrat entre le serveur et les apps (plugin Tauri avec l'aide `@drakbase/tauri`, et paquet Swift `DrakBase`). Toute modification se fait ici d'abord, puis dans `crates/am-protocol` et `fixtures/`. Les choix sont expliqués dans [PLAN.md](PLAN.md).

La dernière section, « Stacks Docker », décrit les stacks Docker, leur registre et l'agent `drakbase-stack`. Elle complète les sections précédentes sans rien y changer pour les apps Swift et Tauri.

## Déroulé

1. L'app demande au serveur s'il existe une version plus récente que la sienne.
2. Le serveur répond 204 (à jour) ou 200 avec la description de la mise à jour.
3. L'app télécharge l'archive et vérifie la signature minisign avec la clé publique qu'elle embarque. La version signée doit être celle annoncée.
4. L'app installe la mise à jour, puis se relance.

Le serveur est compatible avec le mode « serveur dynamique » de `tauri-plugin-updater` v2.

## Vérification

`GET /api/v1/update/{app}/{target}/{arch}/{current_version}`

| Segment | Valeurs |
|---|---|
| `app` | identifiant de l'app (slug) : `[a-z0-9-]{1,64}` |
| `target` | `darwin`, `linux`, `windows` |
| `arch` | `x86_64`, `aarch64`, `i686`, `armv7`, `riscv64` |
| `current_version` | version installée, encodée pour l'URL, lue avec les règles souples de la section « Versions » |

Informations facultatives. Si la même information arrive par paramètre et par en-tête, le paramètre l'emporte.

| Paramètre | En-tête | Valeurs |
|---|---|---|
| `bundle_type` | — | `app`, `nsis`, `msi`, `appimage`, `deb`, `rpm`, `unknown` ; c'est le `{{bundle_type}}` de Tauri. Aussi `stack` et `bin` (section « Stacks Docker ») |
| `channel` | `X-AppsMarket-Channel` | `stable` (par défaut) ou `beta` ; toute autre valeur vaut `stable` |
| `os_version` | `X-AppsMarket-OS-Version` | version de l'OS (`14.6`, `10.0.22631`…), pour écarter les versions qui exigent un OS plus récent |
| — | `X-AppsMarket-Install-Id` | identifiant aléatoire de l'installation (UUID v4), pour les statistiques ; jamais dans l'URL |

**Réponses :**
- `204 No Content` : rien de plus récent.
- `200 OK`, en `application/json` :

  ```json
  {
    "version": "1.2.0",
    "url": "https://maj.example.fr/dl/01JB8Z6T3KQ4N5V7W9X0Y1Z2A3/Translate-1.2.0.app.tar.gz",
    "signature": "dW50cnVzdGVkIGNvbW1lbnQ6…",
    "notes": "- Correction du raccourci clavier",
    "pub_date": "2026-10-24T13:05:00Z",
    "size": 734243,
    "sha256": "9f2c…",
    "min_os_version": "14.0",
    "channel": "stable"
  }
  ```

  - `version`, `url` et `signature` sont exigés par Tauri.
  - `version` est toujours la version canonique.
  - `signature` reprend le contenu du `.sig`, tel quel.
  - `notes` et `min_os_version` sont absents s'ils sont vides.
  - `pub_date` est en RFC 3339 UTC.
  - Tauri ignore les autres champs, qu'il expose dans `rawJson`.
- `400` si la version, la cible, l'arch ou le `bundle_type` sont illisibles ; `404` si l'app est inconnue ou archivée.
  - Un `bundle_type` est illisible quand ce n'est le nom d'aucun type d'artefact : `flatpak`, par exemple, ou un `{{bundle_type}}` que le client n'a pas remplacé. Un type connu qui ne peut pas servir ce client ne donne pas d'erreur, mais 204 (« Choix de la mise à jour », règle 2).
- Un client qui reçoit une version qui n'est pas plus récente que la sienne, ou une bêta alors qu'il suit le canal stable, la traite comme « à jour ». C'est une incohérence du serveur, pas une erreur à montrer.
  - Corps d'erreur : `{"error": "<code>", "message": "<texte>"}`.
- Toutes ces réponses portent `Cache-Control: no-store`.

### Choix de la mise à jour

Implémenté dans `crates/am-server/src/resolve.rs`.

1. **Canal** : `beta` voit les versions stables et bêta, `stable` ne voit que les stables.
2. **Type de bundle**. Tout ce que le serveur fait d'un `bundle_type` lisible est ici :
   - une app `stack` ou `cli` n'a qu'un type, `stack` ou `bin`, quel que soit le `bundle_type` envoyé (section « Stacks Docker »). Il ne sert que la cible `linux` : une autre cible ne reçoit rien (204). Ces deux types ne sont servis à aucune autre app (204) ;
   - si le client n'en envoie pas, ou envoie `unknown` :
     - `darwin` donne `app` ;
     - `windows` donne le type par défaut de l'app, sinon le seul type Windows déjà publié, sinon aucune réponse (204) ;
     - `linux` donne le type par défaut de l'app, sinon `appimage` ;
   - sinon, c'est le type envoyé, s'il sert de mise à jour sur la cible. `dmg` n'est jamais une mise à jour, et un type d'un autre OS que la cible ne s'y installe pas : dans ces deux cas, aucune réponse (204) ;
   - `nsis` et `msi` ne sont jamais servis l'un pour l'autre.
3. **Versions candidates** : celles qui sont publiées, dans un canal permis, et strictement supérieures à `current_version`, de la plus haute à la plus basse. On retient la première qui remplit toutes ces conditions :
   - elle a un artefact pour l'OS, l'arch et le type ; sur `darwin`, à défaut, l'artefact `universal` ;
   - si `current_version` est une version connue du serveur et qu'elle déclare ses clés (`trusted_key_ids` non vide), l'artefact est signé par l'une d'elles ;
   - si le client a envoyé `os_version`, le `min_os_version` de l'artefact lui est inférieur ou égal. Les deux se comparent numériquement, composant par composant, un composant absent valant 0 : `14.6.1` > `14.6` > `14.0`. Une `os_version` illisible est ignorée.
4. Si aucune ne convient : 204.

## Téléchargement

`GET` ou `HEAD /dl/{artifact_id}/{filename}`

- `artifact_id` est un ULID (26 caractères en base32 de Crockford). `filename` ne sert qu'à l'affichage : le serveur l'ignore.
- **Accès** :
  - versions publiées : public ;
  - brouillons : session d'administration avec accès à l'app, sinon 404 ;
  - versions retirées : `410 Gone` ;
  - app archivée : 404, quel que soit l'état de la version.
- **En-têtes de réponse** :
  - `Content-Type: application/octet-stream` ;
  - `Content-Disposition: attachment; filename="<nom assaini>"` ;
  - `ETag: "<sha256 en hexadécimal>"` ;
  - `Accept-Ranges: bytes`, avec prise en charge de `Range` ;
  - `Cache-Control: public, max-age=31536000, immutable`, car un artefact publié ne change jamais ;
  - `Content-Security-Policy: sandbox`.
- **Plages** : une seule plage par requête, servie en `206` avec `Content-Range`. Un `Range` illisible, ou à plusieurs plages, est ignoré : le fichier part en entier. Une plage hors du fichier donne `416`, code `range_not_satisfiable`, avec `Content-Range: bytes */<taille>`.
- **Comptage** : `?src=page` marque un téléchargement depuis la page publique, tout le reste compte comme une mise à jour. Une requête n'est comptée que sans `Range`, ou avec un `Range` qui commence à 0.

## Catalogue (page publique)

| Route | Réponse |
|---|---|
| `GET /api/v1/site` | `{"title": "…"}`, le titre de la page publique (réglage `public_title`) |
| `GET /api/v1/catalog` | tableau des apps visibles : listées et non archivées |
| `GET /api/v1/catalog/{app}`, avec `?channel=beta` en option | fiche d'une app visible ; 404 `not_found` si l'app est inconnue, archivée ou non listée |
| `GET /api/v1/catalog/{app}/icon.png` | `image/png` ; 404 sans icône |
| `GET /api/v1/catalog/{app}/screenshots/{id}.png` | `image/png`, avec `Cache-Control: public, max-age=31536000, immutable` (une capture ne change jamais, elle se remplace) ; 404 si l'app n'est pas visible ou la capture inconnue |

**Élément de la liste :**

```json
{
  "slug": "translate",
  "name": "Translate",
  "description": "Traduction rapide",
  "icon_url": "https://maj.example.fr/api/v1/catalog/translate/icon.png",
  "latest": {
    "version": "1.2.0",
    "channel": "stable",
    "pub_date": "2026-10-24T13:05:00Z",
    "downloads": [
      {
        "os": "darwin",
        "arch": "universal",
        "bundle_type": "dmg",
        "url": "https://maj.example.fr/dl/01JB8Z6T3KQ4N5V7W9X0Y1Z2A3/Translate-1.2.0.dmg?src=page",
        "size": 734243,
        "sha256": "9f2c…",
        "min_os_version": "14.0"
      }
    ]
  }
}
```

- `icon_url` vaut `null` sans icône.
- `latest` donne la dernière version stable publiée, ou `null` s'il n'y en a pas.
- `downloads` donne le meilleur installeur de chaque plateforme (`os` et `arch` tels que publiés, `universal` compris) :
  - `dmg` plutôt que `app` ;
  - `nsis` plutôt que `msi` ;
  - `appimage`, puis `deb`, puis `rpm`.

  La liste est triée par OS (darwin, windows, linux), puis par arch.
- `min_os_version` est absent quand il est inconnu ; `sha256` est en hexadécimal.
- Toutes les URL sont absolues, construites avec `AM_PUBLIC_URL`.

**Fiche d'une app** : les mêmes champs, plus :
- `latest.notes`, les notes en Markdown de cette version (chaîne, éventuellement vide). Avec `?channel=beta`, `latest` est la plus haute version publiée, stable ou bêta, et `latest.channel` le dit.
- `history`, les 20 dernières versions stables publiées, de la plus récente à la plus ancienne, chacune sous la forme `{version, pub_date, notes}`.
- `screenshots`, les captures d'écran dans l'ordre d'affichage, chacune sous la forme `{url, width, height, caption}` ; un tableau vide s'il n'y en a pas. `caption` est du texte brut, éventuellement vide.

## Signatures

minisign au format préhaché « ED » : Ed25519 appliqué au BLAKE2b-512 du fichier. Les textes sont encapsulés exactement comme le fait Tauri.

**Clé publique** : base64 standard du texte complet de la clé minisign :

```
untrusted comment: minisign public key: <key_id en hexadécimal>
<base64("Ed" ‖ key_id(8 octets) ‖ clé Ed25519(32 octets))>
```

**Signature** : base64 standard du texte complet du `.sig` :

```
untrusted comment: signature from tauri secret key
<base64("ED" ‖ key_id(8 octets) ‖ Ed25519(BLAKE2b-512(fichier))(64 octets))>
trusted comment: timestamp:<secondes Unix>	file:<nom du fichier>	version:<version canonique>
<base64(Ed25519(signature(64 octets) ‖ texte du commentaire de confiance))>
```

- Les champs du commentaire de confiance sont séparés par des tabulations, au format `clé:valeur`.
- La deuxième signature, dite globale, est la seule qui authentifie ce commentaire.

**Lecture d'un texte minisign** (clé ou signature) :
- Les lignes sont séparées par `\n` ; un `\r` en fin de ligne est retiré. Aucune ligne n'est rognée ni sautée, et les lignes au-delà de celles décrites ci-dessous sont ignorées.
- La première ligne doit commencer par `untrusted comment:`.
- La deuxième, prise telle quelle, est la ligne de données, en base64 standard strict : remplissage canonique, aucun blanc.
- Pour une signature, la troisième commence par `trusted comment: `, et la quatrième est la signature globale en base64.

C'est le découpage de la crate `minisign-verify`, plus l'exigence sur la première ligne : un texte décalé par une ligne vide est refusé, au lieu de prendre l'identifiant de clé sur une ligne et la clé sur une autre. Les cas sont dans `fixtures/signatures.json`, section `parsing`.

L'enveloppe base64 peut être entourée de blancs à la lecture. Le serveur, lui, enregistre et renvoie la signature sans aucun blanc, car le plugin Tauri ne les retire pas.

**Vérification, dans cet ordre obligatoire** (client comme serveur) :
1. L'algorithme est `ED`. Le format historique `Ed`, non préhaché, est refusé.
2. Le `key_id` de la signature est égal à celui de la clé.
3. La signature Ed25519 du BLAKE2b-512 du fichier est valide.
4. La signature globale est valide.
5. Seulement ensuite, on lit `version:` dans le commentaire de confiance. Elle doit être égale à la version annoncée par le serveur et, côté Swift, à la version du bundle extrait : son `CFBundleShortVersionString`, normalisé comme une version de client. Un bundle en « 1.2 » est ainsi signé « 1.2.0 ».

**Clés :**
- Une paire de clés par app. Avec une clé commune, un serveur compromis pourrait servir une app aux utilisateurs d'une autre.
- Clé principale et clé de secours : une app Swift embarque plusieurs clés publiques et vérifie avec celle dont l'identifiant est celui de la signature. Si aucune ne correspond, c'est `key_id_mismatch`. Le plugin Tauri n'accepte qu'une clé.
- La clé secrète suit le format Tauri : base64 du texte de la clé secrète minisign, chiffrée avec scrypt. Le même fichier sert donc de `TAURI_SIGNING_PRIVATE_KEY`.
- Rotation : une version signée avec l'ancienne clé embarque la nouvelle (`trusted_key_ids`), puis la nouvelle devient active.

## Versions

**Normalisation** (`am_protocol::version`) :
- Les espaces et un `v` ou `V` initial sont retirés.
- Une version à 1 composant prend `.0.0`, une version à 2 composants prend `.0`. À partir de 4 composants, elle est refusée.
- Les composants sont des entiers sans zéro initial, comme en SemVer.
- Une pré-version est permise (`1.4.0-beta.2`).
- Les métadonnées de build (`+…`) sont refusées pour une version publiée, et ignorées dans une version envoyée par un client.

La forme canonique, sans `v` et à 3 composants, est la seule que le serveur annonce, et la seule que l'outil signe : Tauri lit les versions strictement.

Pour une pré-version d'une app Swift, `CFBundleShortVersionString` porte la pré-version (`1.2.0-beta.1`). En effet, le serveur à l'envoi, puis l'app avant l'installation, comparent la version du bundle à celle de la publication.

**Ordre** : précédence SemVer 2.0.0, §11.

Les cas de test communs à Rust et Swift sont dans `fixtures/versions.json`.

### Versions d'OS

`os_version`, `min_os_version` et, pour une stack, la version de Docker Compose sont des nombres séparés par des points (`am_protocol::OsVersion`). Les cas communs à Rust, à Swift et à l'aide Tauri sont dans `fixtures/os-versions.json`.

- **Comparaison** : composant par composant, un composant absent valant 0. `14.6.1` > `14.6` > `14.0`, et `14` égale `14.0.0`.
- **Lecture souple**, pour ce qu'envoie une machine : les blancs autour sont retirés, puis les nombres de tête comptent. La lecture s'arrête au premier composant qui n'est pas fait que de chiffres, dont elle garde les chiffres de tête : `6.8.0-45-generic` vaut `6.8.0`, `2.29.1-desktop.1` vaut `2.29.1`, et `2..1` vaut `2`.
- **Texte illisible** : celui qui ne commence pas par un chiffre (`v2.29`, `.5`, un texte vide), qui dépasse 64 octets, ou dont un nombre ne tient pas sur 64 bits. Une `os_version` illisible est ignorée, et un `min_os_version` illisible n'écarte rien. L'agent retire lui-même le `v` que Docker Compose peut écrire devant sa version.
- **Lecture stricte**, pour ce qu'un développeur déclare (`min_compose_version` de `stack.json`) : de 1 à 4 nombres de 1 à 9 chiffres, et rien d'autre.

## Plateformes et artefacts

| OS | Types | Mise à jour | Première installation |
|---|---|---|---|
| `darwin` | `app` (`.app.tar.gz`), `dmg` | `app` | `dmg`, sinon `app` |
| `windows` | `nsis` (`.exe`), `msi` | même type que l'installation | `nsis`, sinon `msi` |
| `linux` | `appimage`, `deb`, `rpm` | même type que l'installation | `appimage`, puis `deb`, puis `rpm` |

- L'arch `universal` n'existe que sur `darwin` : un binaire universel sert `aarch64` et `x86_64`.
- L'archive `.app.tar.gz` contient un seul dossier `Nom.app/` à sa racine, avec les liens symboliques conservés et sans fichiers AppleDouble `._*`.

## Statistiques

- **Côté client** :
  - l'identifiant d'installation est un UUID v4 aléatoire, créé au premier lancement et rangé localement, jamais dérivé du matériel ;
  - il n'est envoyé que si l'utilisateur laisse les statistiques activées ;
  - Pli ne l'envoie jamais.
- **Côté serveur** :
  - l'identifiant reçu doit être un UUID (8-4-4-4-12 chiffres hexadécimaux) ; il est mis en minuscules, et toute autre valeur est ignorée ;
  - seul `HMAC-SHA256(clé du jour, app ‖ identifiant)`, tronqué à 16 octets, est gardé. L'app y entre par son identifiant en base, sur 8 octets gros-boutiens, suivi des 36 caractères de l'identifiant ;
  - une installation compte une fois par jour (UTC) et par app, sous la dernière version, le dernier canal et la dernière plateforme vus ce jour-là ;
  - chaque vérification est comptée, avec ou sans identifiant, par jour, version, canal, OS, arch et type de bundle : ce compteur ne désigne aucune installation ;
  - chaque nuit, il agrège la journée close, puis efface ces empreintes et la clé du jour ;
  - les copies de la base (sauvegardes, copie d'avant migration) ne contiennent ni empreintes ni clés du jour : une copie gardée des mois permettrait sinon de relier les jours entre eux ;
  - aucune adresse IP n'est conservée.

## Intégration

**Tauri** (`tauri.conf.json`) :
- `requireSignedVersion` existe à partir de `tauri-plugin-updater` 2.14, et `tauri build` (tauri-cli 2.12 et plus) écrit `version:` dans le commentaire de confiance. C'est vérifié avec les fichiers de `fixtures/tauri/`.
- Le plugin accepte encore le format historique « Ed », mais le serveur le refuse à l'envoi : une app n'en reçoit donc jamais.

```json
{
  "bundle": { "createUpdaterArtifacts": true, "windows": { "nsis": { "installMode": "currentUser" } } },
  "plugins": {
    "updater": {
      "pubkey": "<clé publique de l'app>",
      "endpoints": ["https://maj.example.fr/api/v1/update/mon-app/{{target}}/{{arch}}/{{current_version}}?bundle_type={{bundle_type}}"],
      "requireSignedVersion": true,
      "windows": { "installMode": "passive" }
    }
  }
}
```

En JavaScript, le canal, la version du système et l'identifiant d'installation passent en en-têtes : `check({ headers: { "X-AppsMarket-Channel": "beta", "X-AppsMarket-OS-Version": "14.6.1", "X-AppsMarket-Install-Id": id } })`.

L'aide `@drakbase/tauri` (`clients/tauri`) les envoie elle-même. Elle lit la version du système par le plugin officiel `tauri-plugin-os`, que l'app enregistre avec `tauri-plugin-updater`, `tauri-plugin-process` et `tauri-plugin-store` : sans lui, l'en-tête ne part pas.

**Swift** (`Info.plist` signé de l'app) : les clés portent le nom `DrakBase…`. Les anciennes clés `AppsMarket…` (`AppsMarketFeedURL`, `AppsMarketPublicKeys`, `AppsMarketTrustedRequirements`, `AppsMarketInstallerService`) restent lues, chacune seulement quand sa nouvelle clé manque : une app configurée avant le renommage continue de fonctionner.

| Clé | Type | Contenu |
|---|---|---|
| `DrakBaseFeedURL` | chaîne | `https://maj.example.fr/api/v1/update/translate` ; le paquet ajoute la cible, l'arch et la version |
| `DrakBasePublicKeys` | tableau de chaînes | clé principale, puis clé de secours, au format Tauri |
| `DrakBaseTrustedRequirements` | tableau de chaînes, facultatif | exigences de signature de code acceptées en plus de celle de l'app installée |
| `DrakBaseInstallerService` | chaîne, facultatif | identifiant du service XPC d'installation, pour une app sandboxée |

Le paquet envoie le canal et la version de macOS en paramètres (`channel`, `os_version`), et l'identifiant d'installation en en-tête.

**Règles communes aux deux clients**, le paquet Swift et l'aide Tauri :
- **Recherche** : au lancement, puis toutes les 24 heures par défaut (entre une heure et trente jours), comptées depuis la dernière tentative, réussie ou non. Une recherche dure 60 secondes au plus.
- **Téléchargement** : 4 heures au plus.
- **Réponse** :
  - un `channel` qui n'est ni `stable` ni `beta` fait refuser la réponse ;
  - une bêta reçue sur le canal stable, une version qui n'est pas plus récente, ou qui exige un système plus récent que celui de la machine (`min_os_version`, comparé comme dans « Versions d'OS »), compte comme « à jour ».
- **Canal changé pendant une recherche** : la réponse valait pour l'ancien canal. Elle est écartée, et la recherche refaite.
- **Recherche qui échoue** : une version déjà proposée reste connue, et reste à installer. Une recherche automatique se tait alors ; une recherche demandée dit son erreur.
- **Version retirée pendant son téléchargement** (`410`) : elle est oubliée, et la suite est une nouvelle recherche.
- **Identifiant d'installation** : couper les statistiques l'efface ; les rouvrir en tire un nouveau.

**Ce qui diffère** :
- **Version du système** : le paquet Swift la lit toujours, et contrôle en plus `LSMinimumSystemVersion` du nouveau bundle avant de l'installer. L'aide Tauri ne la connaît que par le plugin os : sans lui, elle n'envoie rien et n'écarte rien.
- **Délai du téléchargement** : le paquet Swift coupe aussi après 60 secondes sans octet reçu ; le plugin Tauri n'a que le délai total.
- **Changement de canal, hors recherche** : l'aide Tauri oublie la version affichée dans tous les cas ; le paquet Swift seulement une bêta, en revenant au canal stable.
- **Intervalle nul** : l'aide Tauri ne cherche alors qu'au lancement et à la demande ; le paquet Swift le ramène à une heure, et se coupe par son propre réglage (`automaticChecks`).
- **Horloge reculée** : l'attente ne dépasse pas un intervalle dans le paquet Swift, un intervalle et une heure dans l'aide Tauri, qui se réveille toutes les heures.
- **Propres au paquet Swift** : la version ignorée, l'installation à la fermeture de l'app, la clé de secours (« Signatures ») et le contrôle de la signature de code.

## Stacks Docker

Une stack est un ensemble de conteneurs décrit par un fichier Compose, publié comme les autres apps. L'agent `drakbase-stack` l'installe et la met à jour sur une machine Linux. Les choix sont expliqués dans [PLAN.md](PLAN.md), « Stacks Docker » ; un exemple complet est dans `examples/stack-demo`.

### Vérification et artefacts

- **Route** : l'agent utilise la route de vérification habituelle, `GET /api/v1/update/{app}/linux/{arch}/{version installée}?bundle_type=stack&channel=…&os_version=<version de Docker Compose>`.
  - Il envoie `X-AppsMarket-Install-Id` si l'utilisateur laisse les statistiques activées.
  - Pour une première installation, la version installée vaut `0.0.0`.
- **Deux nouveaux types d'artefacts**, sur `linux` seulement, et sans arch `universal` :
  - `stack` : l'archive d'une stack, `<app>-<version>-<arch>.stack.tar.gz`. Elle sert à la fois à la mise à jour et à la première installation ;
  - `bin` : l'exécutable d'une app de type `cli`, `<app>-<version>-<arch>`, comme l'agent lui-même.
- **Choix du type** : une app `stack` n'a que le type `stack`, une app `cli` que le type `bin`, quel que soit le `bundle_type` envoyé.
- **Version de Docker Compose** : pour une archive `stack`, `min_os_version` est la version minimale de Docker Compose (`min_compose_version` de `stack.json`), et `os_version` est celle de la machine. Elles se comparent comme des versions d'OS.
- **Archs et plateformes Docker** :

| Arch DrakBase | Plateforme Docker |
|---|---|
| `x86_64` | `linux/amd64` |
| `aarch64` | `linux/arm64`, variante `v8` ou absente |
| `armv7` | `linux/arm/v7` |
| `riscv64` | `linux/riscv64` |
| `i686` | `linux/386` |

- **Catalogue** : une app `stack` a ses archives `stack` dans `downloads`, plus `install_key`, la clé publique active au format Tauri. `install_key` est absent pour une app qui n'est pas une stack, et pour une stack sans clé active. La page publique montre la commande d'installation au lieu d'un bouton de téléchargement.

### Archive `stack.tar.gz`

- C'est une archive tar compressée en gzip. Elle contient, à la racine, sans dossier englobant :
  - `stack.json` (obligatoire) ;
  - `compose.yaml` (obligatoire) ;
  - `.env.example` (facultatif) : les réglages que l'utilisateur peut changer, avec leur valeur par défaut. Aucun nom n'y commence par `COMPOSE_` ni `DOCKER_` (`env_name_reserved`) : Docker Compose lit ses propres réglages dans le fichier que lui donne l'agent, et une stack ne les choisit pas. Chaque ligne y est vide, un commentaire (`#`) ou une seule `NOM=valeur`, précédée ou non de `export`, la valeur tenant sur la ligne. Sont refusés (`env_line_unsupported`) : la forme `NOM: valeur`, un nom seul, une valeur entre guillemets ou apostrophes qui ne se referme pas sur sa ligne, autre chose qu'un commentaire après le guillemet fermant (Docker Compose y lirait une seconde variable), et tout caractère de contrôle autre que la tabulation. L'agent lit ce fichier ligne par ligne pour compléter le `.env` de la machine : il doit y voir les mêmes variables que Docker Compose ;
  - `files/` (facultatif) : des fichiers de configuration, que le compose monte en lecture seule.
- **Contenu permis** (`am_protocol::stack::archive`, cas dans `fixtures/stack/archives.json`) :
  - des fichiers ordinaires et des dossiers seulement : ni lien, ni périphérique, ni tube ;
  - des chemins relatifs en UTF-8, faits de composants ordinaires : ni `/` initial, ni `.`, ni `..`, ni composant vide, ni `\`, ni composant qui commence par `._` ;
  - à la racine, rien d'autre que les quatre noms ci-dessus ; `stack.json`, `compose.yaml` et `.env.example` sont des fichiers, et `files` un dossier ;
  - chaque chemin une seule fois ;
  - au plus 1000 entrées, et 16 Mio pour le flux tar décompressé, en-têtes compris ;
  - 17 Mio au plus pour l'archive compressée (`MAX_ARCHIVE_BYTES`, soit 1 Mio de marge pour gzip) : l'outil avant l'envoi, le serveur à la réception et l'agent au téléchargement refusent un fichier plus gros sans l'ouvrir ;
  - les droits notés dans l'archive sont ignorés : l'agent extrait en 0644 et 0755.
- **Archive reproductible**, écrite par `am_protocol::stack::archive::write` :
  - entrées dans l'ordre de leurs chemins (celui d'un dossier finit par `/`), chaque dossier parent ayant la sienne. À la lecture, un dossier peut ne pas avoir d'entrée ;
  - format ustar, dates à 0, propriétaire et groupe 0 sans nom, droits 0755 et 0644 ;
  - gzip au niveau 9, sans nom ni date dans son en-tête.

  Les mêmes fichiers donnent les mêmes octets.
- **Signature** : comme pour tout artefact (section « Signatures »), avec la clé de l'app.

### `stack.json`

```json
{
  "format": 1,
  "app": "home-manager",
  "version": "0.2.0",
  "service": "home-manager",
  "health_timeout": 120,
  "rollback": "manual",
  "pre_update": "backup",
  "min_compose_version": "2.24.0",
  "platforms": ["linux/arm64", "linux/amd64"],
  "public_keys": ["<clé principale au format Tauri>", "<clé de secours>"],
  "images": { "home-manager": "sha256:3f1c…" }
}
```

| Champ | Rempli par | Contenu |
|---|---|---|
| `format` | développeur | le nombre `1` |
| `app` | outil | slug de l'app, `[a-z0-9-]{1,64}` |
| `version` | outil | version canonique, égale à la version publiée et à la version signée |
| `service` | développeur | service dont l'état de santé décide du succès. Sans contrôle de santé, dans le compose ou dans l'image, `--wait` n'attend que son démarrage |
| `health_timeout` | développeur, facultatif | attente de l'état sain, en secondes : un entier de 10 à 900, 120 par défaut |
| `rollback` | développeur, facultatif | `safe` : l'agent revient seul à la version précédente après un échec ; `manual` (par défaut) : il s'arrête et l'explique |
| `pre_update` | développeur, facultatif | service lancé avant le redémarrage, par exemple une sauvegarde ; distinct de `service` |
| `min_compose_version` | développeur, facultatif | version minimale de Docker Compose, de 1 à 4 nombres séparés par des points (`2.24` ou `2.24.0`) |
| `platforms` | développeur, facultatif | de 1 à 5 plateformes Docker, chacune de la table ci-dessus et une seule fois ; `["linux/arm64", "linux/amd64"]` par défaut. L'outil les construit |
| `public_keys` | `drakbase init` | de 1 à 4 clés publiques au format Tauri, d'identifiants distincts : celles qui vérifieront la version suivante. Leurs identifiants sont les `trusted_key_ids` de la version |
| `images` | outil | de 1 à 16 images servies par DrakBase : nom de l'image → empreinte de son index ou de son manifeste |
| `app_control` | développeur, facultatif | chemin absolu, dans le conteneur de `service`, où l'agent monte le dossier de contrôle des mises à jour (`/run/drakbase`, par exemple). L'app sait alors proposer ses mises à jour dans son interface (« Mises à jour validées dans l'app ») |

- **Noms** :
  - un nom de service suit `[A-Za-z0-9._-]{1,64}` ;
- `app_control` commence par `/`, n'est pas `/` seul, fait 255 caractères au plus, et ses composants sont ordinaires (ni vide, ni `.`, ni `..`), faits de lettres, de chiffres, de `.`, `_` et `-` : l'agent écrit ce chemin dans son propre fichier Compose, où un `$` serait interpolé ;
  - un nom d'image suit `[a-z0-9]+([._-][a-z0-9]+)*`, 64 caractères au plus ;
  - une empreinte s'écrit `sha256:` suivi de 64 caractères hexadécimaux minuscules.
- **Deux lectures** (`am_protocol::stack::manifest`, cas dans `fixtures/stack/manifests.json`) :
  - **source** : le `stack.json` du projet, que lit l'outil. Les champs que remplit l'outil (`app`, `version`, `images`) y sont refusés ;
  - **publiée** : celle de l'archive, que lisent le serveur et l'agent. Ces trois champs y sont obligatoires, et `app` et `version` doivent être ceux attendus.
- **Champs** : un champ inconnu ou d'un mauvais type est refusé. L'agent refuse aussi un `format` qu'il ne connaît pas.

### Compose

Le compose est lu deux fois, chez le développeur (par l'outil) puis sur la machine (par l'agent). Tel qu'il est écrit dans l'archive d'abord, comme du JSON (« Compose de l'archive », plus bas) : l'outil et l'agent n'ont pas de lecteur YAML. Puis par Docker Compose lui-même, dans la forme résolue qu'il donne avec `docker compose -p <app> --project-directory <dossier> --env-file <.env> -f compose.yaml --profile '*' config --format json`. Le serveur ne le lit pas.
- **Profils** : `--profile '*'` fait apparaître tous les services, profils compris.
- **Forme résolue** :
  - les variables du `.env` y sont remplacées ;
  - les chemins relatifs y sont absolus, à partir du dossier du projet ;
  - les ports y sont des objets.
- **Variantes** : Compose 2.29 écrit les `devices` en chaînes (`"/dev/ttyUSB0:/dev/ttyUSB0"`), Compose 5.5 en objets. Les deux formes sont lues. Les sorties réelles de ces deux versions sont dans `fixtures/stack/compose/`.

L'analyse (`am_protocol::stack::compose`, cas dans `fixtures/stack/compose.json`) prend cette sortie, le slug de l'app, l'hôte du serveur (`updates.drakbase.com`, avec son port s'il en a un), le dossier du projet, `service`, `pre_update` et `images`. Elle rend la liste des permissions et des avertissements, ou la première règle enfreinte.

**Règles** (sinon refus, avec ce code) :
- **Images** :
  - chaque service a une `image` (`image_missing`) et pas de section `build` (`build_present`) ;
  - l'image est épinglée par empreinte, `…@sha256:<64 hex>` (`image_not_pinned`) ;
  - une image de l'hôte du serveur s'écrit exactement `<hôte>/<app>/<image>@<empreinte>`, sans étiquette (`image_malformed`), pour cette app (`image_other_app`). Elle figure dans `images` avec cette empreinte (`image_not_declared`) ;
  - chaque image de `images` sert au moins à un service (`image_unused`) ;
  - les autres images (Docker Hub…) sont permises, épinglées elles aussi.
- **Services** :
  - `service` existe (`service_missing`) et n'a pas de profil ;
  - `pre_update`, s'il est donné, existe (`pre_update_missing`), et ses profils sont exactement `["drakbase-hooks"]` (`pre_update_profile`), pour que `up` ne le démarre pas ;
  - aucun autre service n'a de profil, `service` compris (`profile_unexpected`).
- **Fichiers de la stack** : un montage `bind`, ou le `file` d'un `config` ou d'un `secret`, qui désigne le dossier du projet doit être dans `files/` (`bind_outside_files`). Un montage `bind` doit en plus être en lecture seule (`bind_not_read_only`). Les données vivent dans des volumes nommés, ou dans un chemin absolu réglé par `.env`, car le dossier de la stack change à chaque version.
- **Chemins montés** : la source d'un montage `bind`, et le `file` d'un `config` ou d'un `secret`, sont faits de composants ordinaires. Un composant `.` ou `..` est refusé (`bind_path_invalid`), dans le dossier du projet comme ailleurs : `<dossier>/files/../../etc` commence comme un fichier de la stack, et en sort. Les barres répétées sont réduites avant toute règle, et dans la valeur d'une permission : Docker Compose garde `//var/run` tel quel, comme `/srv/data//photos` quand un réglage de `.env` finit par une barre.
- **Dossiers de l'agent** : la source d'un montage `bind`, le `file` d'un `config` ou d'un `secret` et le `device` d'un volume ne sont ni `/var/lib/drakbase-stack`, ni `/etc/drakbase-stack`, ni rien de ce qu'ils contiennent, hors `files/` de la version (`compose_value_unsupported`). L'état d'une stack y garde ses permissions acceptées et ses clés de confiance : un conteneur qui l'écrit s'accorderait ce qu'il veut.
- **Liens de la machine** (agent seulement, à chaque contrôle) : ces règles jugent le texte d'un chemin. L'agent résout donc sur la machine la source de chaque montage `bind`, le `file` d'un `config` ou d'un `secret` et le `device` d'un volume ; une source qui mène, par un lien, à un dossier de l'agent ou à la socket d'un moteur de conteneurs sans le dire est refusée, comme un montage `bind` en écriture ou le `device` d'un volume qui mène ainsi à un dossier du système (`/etc`, `/usr`…) : la permission montrée dirait un dossier ordinaire. Limite connue : Docker suit les liens à chaque démarrage d'un conteneur, et un conteneur qui écrit le dossier parent de sa source peut y poser un lien après le contrôle.
- **Champs admis** : la forme résolue ne contient que des champs de la liste ci-dessous. Tout autre champ est refusé (`compose_field_unsupported`, avec son chemin : `services.app.provider`), qu'il soit connu ou non : Compose en ajoute régulièrement qui touchent la machine (`use_api_socket`, `provider`…), et un agent plus ancien ne doit pas les laisser passer sans les comprendre. Un champ dont le nom commence par `x-` est admis partout où cette liste s'applique : Compose l'ignore.
  - **Premier niveau** : `name`, `version`, `services`, `volumes`, `networks`, `configs`, `secrets`.
  - **Service, sans contrôle** : `attach`, `cap_drop`, `command`, `configs`, `cpu_count`, `cpu_percent`, `cpu_period`, `cpu_quota`, `cpu_rt_period`, `cpu_shares`, `cpus`, `cpuset`, `depends_on`, `develop`, `dns`, `dns_opt`, `dns_search`, `domainname`, `entrypoint`, `environment`, `expose`, `extra_hosts`, `group_add`, `healthcheck`, `hostname`, `init`, `labels`, `links`, `mac_address`, `mem_limit`, `mem_reservation`, `mem_swappiness`, `memswap_limit`, `networks`, `pids_limit`, `platform`, `pull_policy`, `read_only`, `restart`, `scale`, `secrets`, `shm_size`, `stdin_open`, `stop_grace_period`, `stop_signal`, `storage_opt`, `sysctls`, `tmpfs`, `tty`, `ulimits`, `user`, `working_dir`. Les `sysctls` y sont parce que le noyau n'accepte dans un conteneur que ceux de ses propres espaces de noms. Les `labels` y sont aussi, sans être une permission, bien que des outils de la machine les lisent (un proxy branché sur Docker, par exemple) : ce qu'ils en font dépend d'eux, pas de la stack.
  - **Service, contrôlés** par les règles de cette section ou la table des permissions : `image`, `build`, `profiles`, `container_name`, `volumes`, `volumes_from`, `env_file`, `label_file`, `network_mode`, `pid`, `ipc`, `uts`, `userns_mode`, `cgroup`, `privileged`, `cap_add`, `devices`, `device_cgroup_rules`, `security_opt`, `ports`, `gpus`, `runtime`, `deploy`, `post_start`, `pre_stop`, `logging`, `blkio_config`, `cpu_rt_runtime`, `oom_kill_disable`, `oom_score_adj`.
  - **Montage d'un service** (élément de `volumes`, forme longue) : `type`, `source`, `target`, `read_only`, `consistency`, `bind`, `volume`, `tmpfs`. Dans `bind` : `propagation`, `create_host_path`, `selinux`, `recursive` (un `selinux` non vide est refusé : Docker changerait l'étiquette du dossier sur la machine, sans permission nouvelle) ; dans `volume` : `nocopy`, `subpath`, `labels` ; dans `tmpfs` : `size`, `mode`.
  - **`logging`** : `driver`, `options`. Sans `driver` (ou vide), c'est le pilote par défaut de la machine qui reçoit les options, et il peut envoyer le journal ailleurs : seules `max-size`, `max-file` et `compress` sont alors admises (`services.app.logging.options.syslog-address` est refusé comme un champ).
  - **Volume de premier niveau** : `name`, `driver`, `driver_opts` (`type`, `o` et `device` seulement), `external`, `labels`. **Réseau** : `name`, `driver`, `driver_opts`, `external`, `attachable`, `enable_ipv4`, `enable_ipv6`, `internal`, `ipam`, `labels`. **Config et secret** : `name`, `file`, `content`, `environment`, `labels`.
  - Sont donc refusés, entre autres : `use_api_socket` (la socket Docker et les identifiants de registre, sans montage visible), `provider` (un programme de la machine), `annotations` (Docker les passe au moteur d'exécution, et runc fait de `org.systemd.property.*` des propriétés de l'unité systemd du conteneur), `external_links`, `cgroup_parent`, `credential_spec`, `isolation`, et la section `models`.
- **Valeurs admises** (sinon `compose_value_unsupported`, avec le champ et sa valeur) :
  - `network_mode` : `bridge`, `none`, `service:<service>`, et `host` ou `container:<nom>`, qui sont des permissions ;
  - `pid` : `service:<service>`, et `host` ou `container:<nom>` (permissions) ; `ipc` : `none`, `private`, `shareable`, `service:<service>`, et `host` ou `container:<nom>` (permissions) ;
  - `uts` et `userns_mode` : `host` seulement (permission) ; `cgroup` : `private`, ou `host` (permission) ;
  - le `type` d'un montage : `bind`, `volume` ou `tmpfs` ;
  - le `driver` de `logging` : `json-file`, `local` ou `none`. Avec les autres (`syslog`, `fluentd`, `awslogs`…), c'est le service Docker qui se connecte, depuis la machine, à l'adresse que donne la stack ;
  - les `driver_opts` d'un volume, qui disent ce que Docker monte : `type` vaut `none`, et rien d'autre. `nfs` et `cifs` sont refusés : Docker passe `o` à mount(2), et `{type: nfs, o: bind, device: /etc}` monte le dossier de la machine sous le nom d'un partage ; un partage se monte sur la machine, et arrive à la stack comme un dossier de l'hôte. `proc` monterait le procfs de la machine, `ext4` un de ses disques. `device` est un chemin absolu et `o` une liste d'options prises parmi `bind`, `rbind`, `ro`, `rw`, `nosuid`, `nodev` et `noexec`, dont `bind` ou `rbind` (donc sans `|`). Le volume est alors une permission `volume_host_path` ;
  - la `propagation` d'un montage `bind` : `private`, `rprivate`, `slave` ou `rslave`. `shared` et `rshared` feraient remonter vers la machine ce que le conteneur monte ;
  - le `recursive` d'un montage `bind` : `enabled`, `disabled`, `readonly` ou `writable`. Avec `writable`, un montage `read_only` laisse ses sous-montages en écriture : il n'est pas en lecture seule (`bind_not_read_only` dans `files/`, clé `:rw` ailleurs) ;
  - l'`ipam` d'un réseau : sans `config` (des adresses choisies par la stack pourraient recouvrir le réseau local ; refusé comme un champ, `networks.<clé>.ipam.config`), et sans autre `driver` que `default` ;
  - le `driver` d'un volume : `local` ; celui d'un réseau : `bridge`. Les `driver_opts` d'un réseau : `com.docker.network.driver.mtu`, `com.docker.network.bridge.enable_icc` et `com.docker.network.bridge.enable_ip_masquerade` seulement (un autre est refusé comme un champ : le nom d'un pont de la machine, par exemple, y brancherait la stack).
- **Fichiers lus par Compose** : un `env_file` ou un `label_file` d'un service est dans `files/` (`env_file_outside_files`), et son chemin est fait de composants ordinaires (`bind_path_invalid`). Compose recopie sinon un fichier de la machine dans l'environnement du conteneur. Le `subpath` d'un montage de volume aussi (`bind_path_invalid`).
- **Ressources de la machine** (`compose_value_unsupported`) : `oom_kill_disable` vrai, un `oom_score_adj` négatif, un `cpu_rt_runtime` positif et un `blkio_config` qui nomme un disque (`weight_device`, `device_read_bps`, `device_write_bps`, `device_read_iops`, `device_write_iops`) sont refusés. Ils ne donnent aucun accès, mais font passer le conteneur avant le reste de la machine : le noyau tuerait d'autres services avant lui, ou briderait leurs disques.
- **Texte visible** (`compose_text_hidden`, avec le champ et la valeur, rendus visibles) : la valeur d'une permission, son libellé, la valeur d'un avertissement, le nom d'un service, et la clé ou le `name` d'un volume, d'un réseau, d'un `config` ou d'un `secret` ne contiennent aucun caractère de contrôle (C0 dont le saut de ligne, DEL, C1), de sens d'écriture ou invisible (U+200B à U+200F, U+2028 à U+202E, U+2060 à U+2069, U+FEFF). Affichés dans la liste des permissions à accepter, ils pourraient en effacer une ligne (`\u{1b}[1A\u{1b}[2K`) ou en cacher une partie. Les lettres accentuées, le tiret long et l'espace insécable restent admis.

**Compose de l'archive.** Le `compose.yaml` d'une archive est le compose publié, décrit ci-dessous. Il est contrôlé tel quel (`am_protocol::stack::compose::check_archived`, cas `archived` du jeu d'essai), par l'outil avant l'envoi et par l'agent avant `docker compose config` : la forme résolue ne dit plus ce que Compose a lu pour la produire. Compose 5.5 y remplace par exemple `env_file` par les variables du fichier.
- C'est exactement un document JSON : un objet, dont le premier caractère significatif est `{`, sans rien après lui, sans commentaire et sans clé en double (`compose_not_json`). Compose lit ce texte comme du YAML : tout ce que YAML comprend et que JSON ne dit pas est donc refusé, pour que les deux lectures donnent le même compose.
  - Un compose écrit en YAML à la main est refusé : ses ancres et ses fusions (`<<`) ne se contrôlent pas sans le lire comme Compose.
  - Hors du saut de ligne `\n`, aucun caractère que YAML tient pour une fin de ligne, ni marque d'ordre des octets : `\r`, U+0085, U+2028, U+2029 et U+FEFF sont refusés partout dans le fichier. Avec `#\r{…}\r---\n{…}`, Compose lirait le premier objet et un contrôle ligne à ligne le second.
  - Une seule exception, pour les archives déjà publiées : la première ligne qu'écrivait l'outil jusqu'à la 0.5.1, admise à l'octet près (`LEGACY_PUBLISHED_HEADER`), suivie du document JSON. L'outil ne l'écrit plus.
- Pas d'`include` au premier niveau, ni d'`extends` dans un service (`compose_field_unsupported`) : l'outil les a déjà résolus à la publication.
- Un `env_file` ou un `label_file` est un chemin relatif écrit en toutes lettres, dans `files/` : ni variable (`$`), ni `\`, ni composant `.` ou `..`, sauf un `./` initial (`env_file_outside_files`). Autre chose qu'un chemin (un nombre, par exemple) donne `compose_json_invalid`.

**Compose publié** (S3) : c'est la sortie JSON de `docker compose -p <app> --project-directory <dossier> [--env-file .env.example] -f compose.yaml --profile '*' config --format json --no-interpolate --no-path-resolution --no-normalize`, sans le `name` de haut niveau ni aucune section `build`. Le JSON est du YAML valide pour Compose.
- Chaque service construit, et chaque service dont l'`image:` est celle d'un service construit, a `image: <hôte>/<app>/<image>@<empreinte de l'index>`, et perd `pull_policy: build|never`. L'hôte est celui de l'adresse publique du serveur (`public_url` de `whoami`).
- Une image construite prend le nom de son `image:`, sans registre ni étiquette, sinon celui du service.
- Le texte est ce JSON seul, suivi d'un saut de ligne : sans commentaire (« Compose de l'archive »).
- Un montage en syntaxe courte dont la source est une variable (`${DATA}:/data`) est refusé, car Compose le lirait, sans interpolation, comme un volume nommé : il s'écrit en syntaxe longue, avec `type: bind`.

**Avertissement**, sans refus : `container_name`. Le projet Compose porte le slug de l'app, et ses conteneurs en tirent leur nom ; un nom fixe peut entrer en conflit avec un conteneur existant.

**Permissions**, que l'agent fait accepter. Chacune a une clé stable, `<service>|<type>|<valeur>` (service vide pour ce qui concerne tout le projet), et un libellé, affiché `<service> : <libellé>`. Le jeu d'essai `permissions` en montre une de chaque type des sorties réelles, avec sa clé et son libellé ; les autres ont leur cas écrit à la main, avec ses libellés (`labels`) :

| Type | Origine | Libellé |
|---|---|---|
| `network_host` | `network_mode: host` | réseau de l'hôte |
| `network_container` | `network_mode: container:<nom>` | réseau du conteneur <nom> |
| `namespace` | `pid`, `ipc`, `uts`, `userns_mode` ou `cgroup` valant `host` | espace <pid…> de l'hôte |
| `namespace_container` | `pid` ou `ipc` valant `container:<nom>` ; valeur `<pid ou ipc>:<nom>` | espace <pid ou ipc> du conteneur <nom> |
| `volumes_container` | chaque élément `container:<nom>` de `volumes_from` (avec ou sans `:ro`, `:rw`) ; valeur `<nom>`. Un élément qui nomme un service de la stack n'en est pas une | volumes du conteneur <nom> |
| `runtime` | `runtime: <nom>` | moteur d'exécution <nom> |
| `privileged` | `privileged: true`, ou une commande `post_start` ou `pre_stop` avec `privileged: true` | mode privilégié : tous les droits sur la machine |
| `capability` | chaque élément de `cap_add` | capacité <CAP> |
| `device` | chaque élément de `devices` (sa source) ; `gpus`, et chaque élément de `deploy.resources.reservations.devices` : valeur `gpu`, ou chacun de ses `device_ids` | périphérique <chemin, `gpu` ou identifiant> |
| `device_rule` | chaque élément de `device_cgroup_rules` | règle de périphériques <règle> |
| `security_opt` | chaque élément de `security_opt`, sauf `no-new-privileges` à `true` | protection modifiée : <valeur> |
| `docker_socket` | montage `bind` d'une source qui mène à la socket d'un moteur de conteneurs : un fichier nommé `docker.sock`, un fichier `.sock` de `/run` ou de `/var/run`, ou un dossier qui la contient (`/`, `/var`, `/run`, `/var/run`, et sous ces deux derniers `docker`, `containerd`, `podman`, `crio` et `user`) ; de même le `file` d'un `config` ou d'un `secret`, et le `device` d'un volume `none` (service vide) | socket Docker <chemin> : contrôle total de Docker, donc de la machine ; pour un dossier : dossier <chemin>, qui contient la socket Docker : contrôle total de Docker, donc de la machine |
| `host_path` | montage `bind`, `file` d'un `config` ou d'un `secret`, hors du dossier du projet ; valeur `<chemin>:ro` (un montage `read_only` sans `bind.recursive: writable`, un `config` ou un `secret`) ou `<chemin>:rw` | dossier de l'hôte <chemin>, avec « en lecture seule » pour `:ro`. En écriture, un dossier du système (`/etc`, `/root`, `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/boot`, `/dev`, `/proc`, `/sys`, `/var/lib`, `/var/lib/docker`, `/var/lib/containerd`, `/var/spool`, ou ce qu'ils contiennent) porte : dossier de l'hôte <chemin>, en écriture : contrôle de la machine |
| `port` | chaque port publié ; valeur `<adresse>:<port>/<protocole>`, `*` pour toutes les adresses (`0.0.0.0`, `::` ou rien), `auto` pour un port choisi par Docker | port <port>/<protocole> sur toutes les adresses, ou sur <adresse> |
| `external_volume` | volume de haut niveau `external: true` (service vide) | volume existant <nom> |
| `shared_volume` | volume de haut niveau, sans `external`, dont le `name` (à défaut, `<app>_<clé>`) n'est pas `<app>_<clé>`, le nom que Compose lui donne seul (service vide), ou dont la clé contient `_` : `demo_x_data` est aussi le volume `data` d'un projet `demo_x`. Compose monterait sans rien dire le volume du même nom d'une autre stack | volume à nom fixe <nom>, qu'une autre stack peut partager |
| `volume_host_path` | volume de haut niveau avec des `driver_opts` (service vide) ; valeur `<type>\|<o>\|<device>` : le type et les options décident de ce qui est monté, et un autre montage du même `device` est une autre permission | dossier de l'hôte <device> (volume <nom>), avec « en lecture seule » si `o` a `ro`, sans `rw` ni `rbind` (`rbind` laisse ses sous-montages en écriture) |
| `external_network` | réseau de haut niveau `external: true` (service vide) | réseau existant <nom> |
| `shared_network` | réseau de haut niveau, sans `external`, dont le `name` (à défaut, `<app>_<clé>`) n'est pas `<app>_<clé>`, ou dont la clé contient `_` (service vide). Les conteneurs d'une autre stack peuvent s'y trouver | réseau à nom fixe <nom>, qu'une autre stack peut partager |

- **Calcul** : les permissions viennent de la forme résolue, donc après lecture du `.env`. Changer de port dans `.env` fait apparaître une nouvelle permission.
- **Ordre** : elles sont triées par clé, sans doublon.
- **Mise à jour** : l'agent compare les clés à celles déjà acceptées. Une clé nouvelle arrête la mise à jour.

### Registre

Le serveur répond comme un registre en lecture seule : c'est le sous-ensemble de téléchargement de l'OCI Distribution Specification 1.1, qui suffit à `docker pull` et à `docker compose pull`. Il n'y a ni `docker push`, ni étiquettes, ni liste : tout se désigne par empreinte, donc rien de ce qui est servi ne change jamais.

| Route | Réponse |
|---|---|
| `GET /v2/` (et `/v2`) | 200, `{}`, avec `Docker-Distribution-API-Version: registry/2.0` |
| `GET` ou `HEAD /v2/{app}/{image}/manifests/{digest}` | le manifeste, octet pour octet comme il a été envoyé |
| `GET` ou `HEAD /v2/{app}/{image}/blobs/{digest}` | la couche ou la configuration |

- **Segments** :
  - `{app}` est le slug d'une app `stack` non archivée ;
  - `{image}` est un nom d'image ;
  - `{digest}` est `sha256:<64 hex minuscules>`. Une étiquette (`latest`) donne `MANIFEST_UNKNOWN`.
- **Accès** : un objet est servi si au moins une version publiée de l'app déclare cette image (`images`) avec une empreinte qui y mène : l'index, ses manifestes, puis leur configuration et leurs couches. Les brouillons et les versions retirées ne servent rien. Il n'y a pas d'authentification : l'accès est ouvert comme celui de `/dl`.
- **En-têtes** :
  - manifeste : `Content-Type` égal au type enregistré, quel que soit l'`Accept` ;
  - couche : `application/octet-stream`, avec `Accept-Ranges: bytes` et prise en charge de `Range` ;
  - tous : `Docker-Content-Digest`, `Content-Length`, `ETag: "<digest>"` et `Cache-Control: public, max-age=31536000, immutable`.
- **Erreurs**, au format du registre, en `application/json` : `{"errors": [{"code": "…", "message": "<texte en français>", "detail": null}]}`.
  - `NAME_UNKNOWN` (404) : app ou image inconnue ;
  - `MANIFEST_UNKNOWN` et `BLOB_UNKNOWN` (404) : objet absent ou non servi ;
  - `DIGEST_INVALID` (400) : empreinte illisible ;
  - `UNSUPPORTED` (405) : toute autre méthode, et toute autre route sous `/v2/`, dont l'envoi.
- **Détails** (S2) :
  - `Docker-Distribution-API-Version` est sur toutes les réponses sous `/v2`, erreurs comprises, et les erreurs portent `Cache-Control: no-store` ;
  - `If-None-Match` donne 304 ; une plage hors de la couche donne 416, avec `Content-Range: bytes */<taille>` et le code `RANGE_INVALID` ;
  - la route des manifestes ne sert que des manifestes, celle des couches que des couches et des configurations ;
  - un nom qui n'est pas `<app>/<image>` donne `NAME_UNKNOWN` ; une étiquette donne `MANIFEST_UNKNOWN`, avant toute recherche de l'app ; une erreur interne donne 500, code `UNKNOWN` ;
  - le registre est soumis au quota par adresse IP des routes publiques (120 requêtes par minute, rafale de 60) : une stack d'une image en fait 7 au premier `pull`. Au-delà, 429 `TOOMANYREQUESTS`, avec `Retry-After`.
- **Statistiques** : les requêtes du registre ne sont pas comptées. Les installations se comptent à la vérification, et les téléchargements à celui de l'archive.
- **Caddy** ne compresse pas `/v2/`.
- **HTTPS** : le client Docker l'exige. Avec le stockage d'images containerd, il le fait même pour `localhost` et `127.0.0.1`, sans revenir à HTTP (essai S0). Les essais passent donc par Caddy, avec son autorité interne, que le Docker d'essai reconnaît par `/etc/docker/certs.d/<hôte>/ca.crt` (`spikes/stack-registry`).
- **Couches** : l'outil les publie en zstd (`application/vnd.oci.image.layer.v1.tar+zstd`), pour environ un tiers de moins qu'en gzip sur Home Manager. Le serveur accepte aussi gzip et les couches non compressées.

### Agent `drakbase-stack`

Construit en S4 (`crates/am-agent`, voir son README). Exécutables statiques pour Linux aarch64 et x86_64 : `scripts/build-agent.sh`.

**Fichiers sur la machine :**

```
/usr/local/bin/drakbase-stack
/etc/drakbase-stack/config.json                  serveur de self-update, identifiant d'installation, statistiques
/var/lib/drakbase-stack/<app>/                   dossier de la stack, en 0700, à root
/var/lib/drakbase-stack/<app>/state.json         voir ci-dessous
/var/lib/drakbase-stack/<app>/releases/<version>/ contenu de l'archive (la version installée et la précédente)
/var/lib/drakbase-stack/<app>/.env               réglages de l'utilisateur
/var/lib/drakbase-stack/<app>/control/           dossier de contrôle, pour une stack qui a `app_control` (« Mises à jour validées dans l'app »)
```

- **Dossier de la stack** : en 0700, à root. Aucun autre compte de la machine n'y entre : ni pour lire le `.env`, ni pour déposer une demande dans `control/request/`.
  - L'agent le crée ainsi, et l'y remet s'il ne l'est plus, à chaque prise du verrou de la stack et à chaque écriture de `state.json`. Il règle ces droits par le descripteur du dossier, ouvert sans suivre de lien : un lien posé à sa place est une erreur.
  - Un dossier trouvé ouvert à d'autres comptes (celui d'un agent plus ancien, en 0755) est refermé, et ce qui attendait dans `control/request/` est retiré sans être lu, une fois avant de le refermer et une fois après (un agent arrêté entre les deux le retrouve ouvert, et recommence) : un compte de la machine a pu y déposer une demande, que rien ne distingue de celles de l'app. Ce qui reste ouvert : un processus qui tenait déjà un descripteur sur `request/` (ou s'y trouvait) avant la fermeture peut encore y écrire, tant qu'il vit. Il ne peut demander que ce que l'app peut demander (« Mises à jour validées dans l'app »).
  - Les montages des conteneurs n'en dépendent pas : c'est le service Docker, en root, qui les fait.
  - Sans root, une commande ne lit donc plus les stacks : elle s'arrête sur « accès refusé », en demandant `sudo`.
- **`state.json`** : serveur et canal ; versions installée, précédente et en échec, chacune avec les images de son compose résolu (pour supprimer celles qu'aucune version gardée n'utilise plus) ; versions écartées ; clés de confiance ; permissions acceptées.
- **`config.json`** est créé par la première commande qui en a besoin. Son serveur est celui de la première installation (`--server`, sinon le serveur de production) et sert à `self-update` ; chaque stack garde le sien dans `state.json`.
- **Essais** : `DRAKBASE_STACK_CONFIG_DIR`, `DRAKBASE_STACK_STATE_DIR`, `DRAKBASE_STACK_SYSTEMD_DIR`, `DRAKBASE_STACK_DOCKER` et `DRAKBASE_STACK_SYSTEMCTL` remplacent les dossiers et les commandes.

Docker Compose est appelé avec `--project-name <app> --project-directory <app>/releases/<version> --env-file <app>/.env -f <app>/releases/<version>/compose.yaml`. Une seule opération à la fois par stack (verrou `flock`). Docker Compose ne reçoit que des variables choisies : `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TERM`, `NO_COLOR`, `TMPDIR`, `XDG_RUNTIME_DIR`, et de quoi joindre le service Docker : `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG`, `DOCKER_TLS_VERIFY`, `DOCKER_CERT_PATH`, `DOCKER_API_VERSION`, `DOCKER_DEFAULT_PLATFORM`. Une variable du shell l'emporterait sinon sur le `.env` dans l'interpolation, et `COMPOSE_*` changerait le projet. Un compose peut lire ces variables (`environment: ["HOME"]`, `${HOME}`) et les donner à son conteneur : aucune ne porte de secret. Les variables de proxy et `DOCKER_AUTH_CONFIG` ne sont donc pas transmises, car elles portent des mots de passe ; les identifiants d'un registre vont dans la configuration de Docker (`docker login`).

**Prérequis** : Docker Engine 24 et Docker Compose 2.29 au moins, les plus anciens essayés (zstd demande de toute façon Docker 23). L'agent les contrôle avant toute opération, et refuse sinon en le disant. Il fait confiance aux autorités de certification du système.

**Installation** : `drakbase-stack install <app> --key <clé publique> [--server <adresse>] [--channel beta] [--no-stats] [--accept]`
- La clé, au format Tauri, est l'ancre de confiance. Elle arrive par le message du développeur (`drakbase release` affiche la commande), ou par la page publique ; dans ce dernier cas, elle ne vaut que la confiance faite au serveur, comme le premier DMG.
- L'agent refuse si un projet Compose de ce nom existe déjà sans avoir été créé par lui (`docker compose ls --all`).
- Elle suit les étapes 1 à 10 de la mise à jour depuis la version `0.0.0`, sans préparation (il n'y a rien à préparer). Le `.env` est la copie de `.env.example`, ou celui qu'une désinstallation a gardé.
- Toutes les permissions sont montrées. Dans un terminal, l'agent demande de les accepter ; sinon il faut `--accept`, faute de quoi il s'arrête (code 3) sans rien laisser.
- Tout texte venu de la stack ou du serveur que l'agent écrit au terminal ou dans `status.json` (messages, erreurs, permissions, notes de version, fin du journal des conteneurs) passe par `am_protocol::visible` : ses caractères cachés y sont écrits en clair, les sauts de ligne gardés là où le texte a plusieurs lignes.
- Si aucune version n'est publiée pour la machine, c'est un échec (code 1).
- Une installation qui s'arrête avant le démarrage (étapes 1 à 8) ne laisse rien de la stack sur la machine : ni version, ni dossier de contrôle. Seul reste le `.env` qu'une désinstallation avait gardé.
- `--no-stats` coupe les statistiques pour toute la machine (`stats: false` dans `config.json`).

**Mise à jour** : `drakbase-stack update [<app>] [--accept] [--retry]`. Rien n'est jamais installé sans qu'une personne le demande : par cette commande, tapée sur la machine, ou par l'interface de l'app (« Mises à jour validées dans l'app »).
1. **Vérification** (route ci-dessus). Une réponse 204 veut dire que tout est à jour.
2. **Téléchargement** dans `releases/.partial`. La taille et le SHA-256 doivent être ceux annoncés. L'adresse est celle du serveur de la stack (même schéma, même hôte, même port), ou le même hôte en `https`, sur le port 443, quand la stack a été installée en `http`. Aucune redirection n'est suivie, sauf vers une adresse en `https` qui reste celle-là (même hôte et même port, ou le port 443 en venant de `http`) : l'agent ne va chercher un fichier nulle part ailleurs. Un serveur connu sous une autre adresse (un autre nom, une adresse IP) est refusé, avec les deux adresses dans le message : la stack s'installe avec l'adresse que le serveur annonce. La réponse à la vérification fait 1 Mio au plus.
3. **Signature** : vérifiée avec les clés de confiance (`public_keys` de la version installée, ou la clé d'`install`), dans l'ordre de la section « Signatures ». La version signée doit être celle annoncée, et plus haute que celle installée.
4. **Extraction** selon les règles de l'archive, dans un dossier provisoire à côté de celui de la version (`releases/.<version>.next`). Dans `stack.json`, `app` et `version` doivent correspondre.
5. **Contrôle du compose**. Le `compose.yaml` de l'archive est d'abord contrôlé tel quel (« Compose de l'archive »), avant que Docker Compose ne le lise. Puis vient sa forme résolue (`docker compose config`, sur le seul `compose.yaml` de la version), avec le `.env` qu'aura la version : celui de l'utilisateur, complété des variables nouvelles de `.env.example`, préparé à côté (`.env.next`). Les permissions sont donc celles qui démarreront. Une permission nouvelle arrête la mise à jour jusqu'à `update --accept`, tapé sur la machine (code 3). Les permissions acceptées, le dossier provisoire devient `releases/<version>`, par renommage : un dossier de ce nom (une version restée en échec que l'on reprend) est remplacé d'un coup, jamais laissé à moitié écrit. Il est mis de côté (`releases/.<version>.old`) le temps du renommage ; si l'agent est arrêté à ce moment, la commande suivante le remet en place, sous le verrou de la stack, avant de lire `releases/`.
6. **`.env`** : le `.env` préparé remplace l'ancien. Les variables nouvelles y sont ajoutées avec leur valeur par défaut et leurs commentaires. Les autres ne sont jamais modifiées, et une variable que l'utilisateur a mise en commentaire ne revient pas. Une variable qu'il a écrite `NOM: valeur`, forme que Docker Compose lit aussi, est déjà là : sa valeur par défaut n'est pas ajoutée. Seul le succès (étape 10) garde ce `.env` complété : si la version s'arrête ensuite (étapes 7 à 9), le `.env` d'avant revient, octet pour octet, et un `.env` qui n'existait pas est retiré. Seule exception : une première installation qui échoue à l'étape 9 (« Échec à l'étape 9 »).
   - Le `.env` d'avant ne revient tel quel que si le `.env` est encore celui écrit à cette étape. Changé entre-temps (à la main, pendant l'attente de l'état sain), il garde ces changements : seules les lignes des variables que la version avait ajoutées en sont retirées, et l'agent le dit. Retiré entre-temps, il n'est pas remis.
   - Un agent arrêté de force entre cette étape et la fin (coupure, `kill -9`) ne remet rien : le `.env` complété reste, ses variables ajoutées avec leur valeur par défaut, comme après un succès. La version gardée, si elle est relancée, retrouve ainsi ses réglages, et pas ceux qu'une version suivante a choisis pour elle.
7. **`docker compose --profile '*' pull`**, pour tirer aussi l'image de `pre_update` : Docker ne télécharge que les couches qu'il n'a pas, et contrôle chacune par son empreinte. S'il échoue, la version en place continue de tourner.
8. **Préparation** : `pre_update`, s'il existe, par `docker compose … --profile drakbase-hooks run --rm -T <service>`, avec la nouvelle version. S'il échoue, rien n'est redémarré.
9. **Redémarrage** : `docker compose up -d --remove-orphans --wait --wait-timeout <health_timeout>`, qui attend l'état sain des services, dont `service`.
10. **Succès** : la version devient l'installée, et l'ancienne devient la précédente. Les dossiers et les images des versions plus anciennes sont supprimés, ainsi que les clés de confiance remplacées par les `public_keys` de la nouvelle version.

**Échec aux étapes 2 à 8** : la version n'est pas écartée, car rien n'a redémarré ; une nouvelle demande la réessaie. C'est aussi le cas d'une signature refusée, puisque c'est alors le serveur qui se trompe. Son dossier est retiré, sauf celui d'une version restée en échec (`failed`) que l'on reprend par `update --retry` : ses conteneurs sont en l'état, peut-être en marche. Ce dossier reste donc, `failed` ne change pas, et `rollback` reste possible. Jusqu'à l'étape 5 comprise, il n'est pas touché. Après l'étape 6, le `.env` d'avant revient. Un dossier de contrôle créé pour cette version, alors que la version qui tourne n'en a pas, est retiré.

**Échec à l'étape 9** : dans les deux cas, la version est écartée, et le `.env` d'avant revient.
- avec `rollback: "safe"`, l'agent remet le `.env` d'avant, puis relance la version précédente, attend son état sain et le dit. Cette relance est contrôlée comme toute relance d'une version gardée (ci-dessous). Si elle demande une permission nouvelle, le retour seul ne l'accepte pas : rien n'est relancé (code 3), la version reste en échec comme avec `manual`, et l'agent montre les permissions et la commande `sudo drakbase-stack rollback <app> --accept` ;
- avec `manual`, il laisse les conteneurs en l'état, puis montre la fin du journal et la commande `drakbase-stack rollback <app>`. La version est notée en échec (`failed`) : tant qu'elle l'est, `update` le rappelle (code 1), et `restart` est refusé. Ce rappel et ce refus contrôlent la version installée avant de conseiller `rollback` : si elle demande une permission, ils donnent `rollback --accept` ; si son compose n'est plus admis, ils disent ce qui reste possible, comme `rollback` le ferait. Seule une version plus récente s'installe par-dessus ; une plus ancienne demande d'abord `rollback`, qui relance la version installée, la dernière saine. Les variables que la version avait ajoutées au `.env` n'y sont plus : `update --retry` les remet avec leur valeur par défaut, et une autre valeur s'écrit dans le `.env` avant de reprendre ;
- une première installation n'a pas de version à relancer : ses conteneurs sont laissés en l'état, avec le `.env` qui les a démarrés.

**Relance d'une version gardée** (retour seul, `rollback`, `restart`) : elle n'a jamais lieu sans contrôle. Le `.env` a pu changer depuis son dernier démarrage, à la main ou par une version plus récente, et le même compose peut alors demander autre chose à la machine. L'agent refait donc l'étape 5 pour cette version : son `compose.yaml` tel qu'il est écrit (une version installée par un agent plus ancien n'y est jamais passée), puis la forme résolue de ce seul compose avec le `.env` courant (`docker compose config`), les règles d'une stack, et les permissions, comparées à celles déjà acceptées. Une version que ces règles refusent ne repart pas. Une permission nouvelle arrête (code 3) sans rien relancer, jusqu'à `--accept`, tapé sur la machine.
- Une version gardée refusée (installée par un agent plus ancien, qui admettait son compose) ne fait jamais conseiller `rollback`, qui échouerait. Après l'échec d'une version (`safe` ou `manual` : avec `manual`, la version installée est contrôlée pour ce seul conseil), et par `rollback` ou `restart` eux-mêmes, l'agent dit ce qui reste possible : une version plus récente (`update`), la reprise de la version restée en échec (`update --retry`), la correction d'un réglage du `.env` quand c'est la forme résolue qui est refusée, ou `uninstall`. L'interface de l'app reçoit le même conseil dans le message de la demande.

Une version écartée n'est pas reprise seule : `update --retry` la reprend. Elle le reste tant qu'un `rollback` peut ramener sous elle ; seules les versions plus anciennes que la précédente gardée sont oubliées.

**Autres commandes** :
- `check [<app>]` fait les étapes 1 à 5 sans rien installer : la version disponible est téléchargée, sa signature vérifiée, son compose contrôlé avec le `.env` qu'elle aurait, puis tout est retiré. Elle écrit le résultat dans `status.json` (ci-dessous), si la stack a un dossier de contrôle, et le montre, avec la commande pour installer.
- `rollback <app> [--accept]` relance la version précédente et écarte celle qu'il quitte. Il garde les clés de confiance de la dernière version vérifiée. La version relancée est d'abord contrôlée avec le `.env` actuel (« Relance d'une version gardée ») : une permission nouvelle arrête (code 3) jusqu'à `--accept`.
- `restart <app> [--accept]` relance la version installée avec le `.env` actuel, par exemple après un réglage changé à la main. Le compose est d'abord contrôlé avec ce `.env` : une permission nouvelle arrête (code 3) jusqu'à `--accept`. Refusé tant qu'une version est en échec.
- `status`, `logs <app> [-f] [-n N]`.
- `uninstall <app> [--delete-data]` : les volumes et le `.env` ne sont supprimés qu'avec cette option, et une confirmation dans un terminal (le nom de l'app à taper ; sans terminal, refus, code 2). Le `.env` gardé resservira à une nouvelle installation.
  - Avant la confirmation, l'agent nomme les volumes qui partiront (`docker compose down --volumes` : ceux que Compose crée pour la stack, et les volumes sans nom des conteneurs ; jamais un volume `external`). Il signale ceux dont le nom est fixe (permission `shared_volume`) : une autre stack peut y garder ses données, et Docker Compose les efface dès qu'aucun conteneur ne les tient. La personne qui confirme décide en le sachant. Une désinstallation abandonnée à la confirmation ne change rien : le `.env` que l'agent aurait dû créer pour lire le compose (effacé à la main) ne reste pas.
- `auto on|off` installe ou retire les unités systemd de l'agent. Sans systemd, aucune unité ne reste. `auto on` refuse un exécutable qu'un autre compte que root peut remplacer (lui-même ou l'un de ses dossiers appartient à un autre compte, ou est ouvert en écriture au groupe ou à tous, hors dossier à suppression restreinte comme `/tmp`) : le minuteur le lancerait chaque jour en root.
  - **Un minuteur quotidien** (décalage aléatoire d'une heure au plus, rattrapage après un arrêt). Son service lance `-<exe> self-update`, dont l'échec n'empêche pas la suite, puis `<exe> check` pour chaque stack : il vérifie et prévient, sans rien installer.
  - **Une unité `drakbase-stack-request@<app>.path` par stack dont la version qui tourne a `app_control`** (`DirectoryNotEmpty=` sur son dossier `control/request`), qui lance `<exe> request <app>` dès que l'app dépose une demande. La version qui tourne est celle restée en échec, sinon l'installée.
    - `auto on` l'active pour ces stacks, et la désactive pour les autres. Il lit le `stack.json` de la version qui tourne, sous le verrou de la stack ; s'il est illisible, la stack n'a pas d'unité.
    - Ensuite, chaque démarrage réussi l'accorde à la version démarrée, activée avec `app_control`, désactivée sans : `install`, `update`, `update --retry` (après une première installation restée en échec aussi), une demande de l'interface, `rollback` et `restart`. Un échec de systemctl y est un avertissement, pas un échec de la mise à jour.
    - Une désinstallation la retire.

**Codes de sortie** : `0` à jour ou mis à jour, `1` échec, `2` usage, `3` permission à accepter. Pour plusieurs stacks, un échec l'emporte sur une permission à accepter.

**Statistiques** : l'identifiant d'installation est un UUID v4 aléatoire, gardé dans `config.json` et envoyé à chaque vérification, sauf avec `--no-stats`.

### Mises à jour validées dans l'app

Une stack dont `stack.json` a `app_control` propose ses mises à jour dans sa propre interface. L'agent vérifie chaque jour ; l'utilisateur lit les notes, puis valide ; l'agent installe dans la seconde qui suit. Les types de ces fichiers et leurs cas d'essai sont dans `am_protocol::stack::control` et `fixtures/stack/control.json`.

**Dossier de contrôle** : `/var/lib/drakbase-stack/<app>/control/` (0755, à root), monté en lecture seule dans le conteneur de `service` au chemin `app_control`. Son sous-dossier `request/` y est monté à part, en écriture, à `<app_control>/request` : l'app n'écrit que là, même en root dans son conteneur. L'agent ajoute ces deux montages par un fichier Compose à lui, `drakbase-control.json`, écrit dans le dossier de la version et passé par un second `-f` à toutes les commandes Compose, sauf `config` : le contrôle des permissions ne voit que le compose de la stack. Ce fichier donne aussi `DRAKBASE_CONTROL=<app_control>` à l'environnement du service : l'app sait ainsi qu'un agent la met à jour. Sans lui (Synology, archive macOS), l'app ne propose rien.
- **Seulement avec `app_control`** : une stack dont la version n'a pas `app_control` n'a ni dossier de contrôle, ni unité des demandes, et `request` la refuse.
  - Le dossier est créé avec `drakbase-control.json`, avant `pull`, pour une version qui a `app_control`. Si cette version s'arrête avant de démarrer (`pull`, préparation), un dossier créé pour elle repart avec elle.
  - Il est retiré quand la version qui vient de démarrer n'en a pas, après un `up` réussi (mise à jour, `rollback`, `restart`, retour seul après un échec) : ses conteneurs ne le montent plus. `uninstall` le retire aussi, comme une installation qui s'arrête avant le démarrage, et `auto on` pour une stack dont la version qui tourne n'en a pas.
  - Rien d'autre ne le crée : ni `check`, ni l'écriture de `status.json`.
- **Hors de portée des comptes de la machine** : le dossier de la stack est en 0700, à root (« Agent `drakbase-stack` »). `request/` est en 0777 pour l'utilisateur du conteneur, qui y arrive par son montage ; un compte local, lui, n'entre pas dans le dossier de la stack.

**`status.json`**, écrit par l'agent (0644, remplacé d'un bloc) après chaque vérification, chaque demande et chaque changement de version, tant que le dossier de contrôle existe :

```json
{
  "format": 1,
  "app": "home-manager",
  "installed": "0.1.0",
  "channel": "stable",
  "checked_at": "2026-11-13T04:12:00Z",
  "available": {
    "version": "0.2.0",
    "channel": "stable",
    "pub_date": "2026-11-12T16:00:00Z",
    "notes": "- Kodi, Google Cast et Android TV.",
    "new_permissions": []
  },
  "failed": null,
  "operation": {
    "action": "update",
    "version": "0.2.0",
    "state": "done",
    "started_at": "2026-11-13T08:30:02Z",
    "finished_at": "2026-11-13T08:30:41Z",
    "message": "version 0.2.0 en place"
  }
}
```

| Champ | Contenu |
|---|---|
| `installed` | version installée et saine ; `null` avant la première |
| `checked_at` | dernière vérification réussie ; `null` avant la première |
| `available` | version plus récente, téléchargée et vérifiée (signature, version, compose) ; `null` si l'app est à jour |
| `available.notes` | notes de version en Markdown, chaîne éventuellement vide. Ses fins de ligne Windows y deviennent des sauts de ligne, et ses autres caractères cachés (ceux de « Texte visible ») y sont écrits en clair : `\u{1b}` |
| `available.new_permissions` | libellés des permissions nouvelles. S'il y en a, l'interface ne propose pas d'installer : elle montre la commande `sudo drakbase-stack update <app> --accept`, à taper sur la machine |
| `failed` | version restée en échec (`rollback: manual`, ou retour seul qui ne s'est pas fait), sinon `null`. L'interface montre alors la commande `sudo drakbase-stack rollback <app>` |
| `operation` | la dernière demande, ou `null` : `action` (`check` ou `update`), `version` (pour `update`), `state` (`running`, `done`, `failed` ou `permissions`), `started_at`, `finished_at` (`null` pendant `running`), `message` (texte en français, à montrer tel quel) |

**Demandes**, écrites par l'app dans `control/request/` (0777, pour l'utilisateur du conteneur) : un fichier `<nom>.json` par demande, écrit sous un nom qui commence par `.`, puis renommé. 4 Kio au plus.

```json
{ "format": 1, "action": "update", "version": "0.2.0" }
{ "format": 1, "action": "check" }
```

- `request <app>` lit les fichiers `*.json` dans l'ordre des noms, les retire tous (une demande illisible aussi, pour ne pas boucler), puis traite chaque demande lisible. Il attend le verrou de la stack si une autre opération tourne.
- **Refus sans `app_control`** : quand la version qui tourne (celle restée en échec, sinon l'installée) n'a pas `app_control`, `request <app>` refuse (code 1), sans lire ni créer `control/`, et indique `sudo drakbase-stack update <app>`. Il désactive aussi l'unité des demandes de cette stack, si elle était restée activée : elle relancerait `request` tant que le dossier n'est pas vide.
- **Jamais de demande laissée derrière une erreur** : l'unité relance `request` tant que `request/` n'est pas vide. Quand `request` s'arrête avant d'avoir pris les demandes (le `stack.json` de la version qui tourne ne se lit plus, `request/` ne s'ouvre pas), il les retire sans les lire et note l'échec dans `operation` (`action` : `check`, `state` : `failed`, avec son message). Quand rien ne peut être noté (`state.json` illisible, verrou impossible, écriture refusée), il désactive l'unité des demandes de la stack ; le prochain démarrage réussi, ou `auto on`, la réactive.
- **Demande en cours d'écriture** (nom qui commence par `.`) : `request` l'attend quelques secondes, puis la laisse à l'app. Au bout d'une minute, elle est abandonnée, et retirée. Un fichier dont la date de modification est dans le futur, ou illisible, est retiré tout de suite : il ne serait jamais périmé, et l'unité relancerait `request` sans fin, le dossier n'étant jamais vide.
- **Aucun lien suivi** : l'agent tourne en root, et ne se fie pas à ce que contient `control/`.
  - `request` doit être un vrai dossier ; sinon il est retiré, sans que sa cible soit touchée, puis recréé, par `request <app>` et par toute mise à jour ou relance.
  - Une demande est un fichier ordinaire de 4 Kio au plus, lu sans suivre de lien ni attendre un tube. Toute autre entrée (lien, dossier, tube, périphérique) est une demande refusée, retirée elle aussi sans que sa cible soit touchée.
  - `status.json` est préparé hors de `control/`, puis mis en place par renommage.
  - Une stack installée par un agent plus ancien garde `control/` monté en écriture jusqu'à sa prochaine mise à jour ou relance (`restart`, `rollback`) : ces règles valent donc aussi pour elle.
- **Stack sans `app_control` installée par un agent plus ancien** : cet agent créait `control/request/` pour toute stack, dans un dossier en 0755, et lui donnait une unité des demandes. Elle garde son `control/` jusqu'à sa prochaine mise à jour, sa prochaine relance (`restart`, `rollback`) ou le prochain `auto on`. Entre-temps, le dossier de la stack la protège : il est remis en 0700 par la première commande qui prend son verrou (le `check` quotidien, par exemple), qui vide aussi `request/`, et `request` la refuse.
- **`check`** : comme la commande `check`.
- **`update`** : la version demandée doit être `available.version`, sans permission nouvelle ; sinon l'opération échoue, avec un message. L'agent fait alors exactement `update`, la version écartée comprise, puisque l'utilisateur la redemande. Il n'installe que cette version : si le serveur en propose une autre entre-temps, ou plus aucune, rien n'est téléchargé ni installé, et l'opération échoue en demandant de vérifier de nouveau. `operation.state` passe à `running` avant le téléchargement, puis à `done`, `failed` (avec le message de l'agent, retour arrière compris) ou `permissions`. Une version qui n'est pas devenue saine, et dont le retour seul s'est arrêté sur une permission nouvelle de la version précédente, donne `failed` : son message dit la commande à taper sur la machine (`sudo drakbase-stack rollback <app> --accept`), et `failed` porte cette version.
- Une demande illisible, d'un `format` inconnu, ou d'une `version` qui n'est pas canonique (`v0.2`, `0.2`), donne une opération `failed` (`action` : `check`, message « demande refusée : … »), sans rien faire.
- `request` répond 0 si tout s'est bien passé, 1 si une demande a échoué ou a été refusée, et 3 si une permission est à accepter sur la machine : celles de la version demandée, ou celles de la version précédente quand le retour seul s'est arrêté.

**Ce que l'app peut et ne peut pas faire** : demander une vérification, ou l'installation de la version que l'agent a lui-même vérifiée. Tous les contrôles de la mise à jour restent faits. Elle ne peut ni accepter de permission, ni choisir une autre version, ni écrire ailleurs que dans `request/`.

**Côté app** : seuls ses administrateurs voient et valident les mises à jour. Pendant `running`, l'interface relit `status.json` ; l'app redémarre au milieu de l'opération, et l'interface se reconnecte. Home Manager en est le premier exemple (sa page Système).
- **Lecture tolérante** : l'app exige `format: 1`, ignore les champs qu'elle ne connaît pas, et montre un `operation.state` inconnu comme une opération terminée, avec son message. L'agent peut ainsi ajouter un champ sans changer de format ; en retirer un ou en changer le type demande `format: 2`. L'agent, lui, ne relit jamais `status.json` : il l'écrit d'un bloc à partir de son état. Le jeu d'essai (`statuses` de `fixtures/stack/control.json`) fixe la forme exacte de ce qu'il écrit ; sa lecture stricte, « champ inconnu » compris, ne vaut que pour ces essais.
- **Refus avant d'écrire** : l'app refuse ce que l'agent ferait échouer. C'est le cas d'une demande tant qu'une autre attend dans `request/` (elle peut lister ce dossier) ou qu'une opération est `running`. C'est aussi le cas d'un `update` d'une autre version que `available.version`, de la version `failed`, ou d'une version qui a des `new_permissions`. Un double clic ou une page périmée remplacerait sinon le résultat d'une mise à jour réussie par une opération `failed`.
- **Opération interrompue** : une opération `running` commencée il y a plus d'une heure est tenue pour interrompue. L'app accepte alors une nouvelle demande et montre `sudo drakbase-stack status`. De son côté, l'agent réécrit en `failed` (« opération interrompue ») un `running` qu'il trouve sous son verrou : il a été laissé par un processus arrêté. Toute commande qui prend le verrou de la stack le fait, `update`, `rollback` et `restart` tapés sur la machine compris.
- **Agent absent** : une demande que personne ne prend au bout d'une minute laisse penser que les unités de l'agent manquent. L'app montre alors `sudo drakbase-stack auto on`.
- **Nom des demandes** : Home Manager les nomme `<AAAAMMJJTHHMMSS.mmmZ>-<8 hex>.json`, en UTC, ce qui les trie dans l'ordre d'écriture.

### Exécutable `bin` (app `cli`)

- C'est un exécutable ELF, de préférence statique, pour l'arch de l'artefact.
- **`drakbase.json`**, à la racine du projet, que `drakbase init --cli` écrit : `{"app": "<slug>", "public_keys": [<1 à 4 clés au format Tauri, d'identifiants distincts>]}`. L'app embarque ces clés à la compilation (l'agent : `crates/am-agent/drakbase.json`, lu par son `build.rs`). Elles vérifient la version suivante, comme l'Info.plist d'une app Swift.
- **`drakbase-stack self-update`** :
  1. vérification `…/update/drakbase-stack/linux/<arch>/<version>?bundle_type=bin`, sur le canal stable ;
  2. contrôle de la signature avec les clés embarquées, et de la version signée, plus haute que la sienne ; sans clé embarquée, refus expliqué ;
  3. le fichier reçu doit être un ELF, un exécutable (`e_type` `ET_EXEC` ou `ET_DYN` : un fichier objet est refusé), et de la bonne arch ;
  4. écriture à côté de l'exécutable, dans un fichier créé pour l'occasion (ce qui se trouve à sa place, lien compris, est retiré sans être suivi), puis remplacement par renommage, sous un verrou qui empêche deux remplacements simultanés.
- **Première installation** : `GET /install/drakbase-stack.sh` sert un script POSIX (`text/plain; charset=utf-8`, `Cache-Control: no-cache`), dans lequel l'adresse du serveur (`AM_PUBLIC_URL`) est écrite. Il refuse un autre système que Linux et un utilisateur non root, puis :
  1. lit l'arch (`uname -m`) ;
  2. demande `…/update/drakbase-stack/linux/<arch>/0.0.0?bundle_type=bin`, sur le canal stable ; une réponse 204 l'arrête avec un message ;
  3. exige une `url` sous `<AM_PUBLIC_URL>/dl/`, télécharge avec curl (en HTTPS seulement si l'adresse l'est) ou wget, contrôle la taille et le SHA-256 avec sha256sum ou openssl ;
  4. écrit `drakbase-stack.part.<pid>` dans `/usr/local/bin`, puis le renomme en 0755.

  Il ne fait confiance qu'à TLS : la signature n'est vérifiée qu'aux mises à jour suivantes.
