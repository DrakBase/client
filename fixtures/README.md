# Jeux d'essai communs

Fichiers rejoués à l'identique par les tests Rust (`crates/am-protocol/tests/fixtures.rs`) et par ceux du paquet Swift (`clients/swift/DrakBase`). Ils fixent le comportement attendu de chaque implémentation. Ceux de `stack/` ne concernent que Rust (serveur, outil et agent) : `crates/am-protocol/tests/stack_fixtures.rs` les rejoue.

| Fichier | Contenu | Origine |
|---|---|---|
| `versions.json` | normalisation des versions (publication et client) et ordre de précédence | écrit à la main |
| `os-versions.json` | versions d'OS ou de Docker Compose : lecture souple, ordre, égalités et comparaisons à un minimum | écrit à la main |
| `update-response.json` | corps de la réponse 200 de la vérification : cas acceptés, avec les valeurs lues, et cas refusés | écrit à la main |
| `signatures.json` | cas de vérification (`cases`) et de lecture (`parsing`), avec le résultat attendu | généré |
| `blake2b.json` | vecteurs BLAKE2b-512, dont celui de la RFC 7693 | généré |
| `keys/test-app.pub`, `keys/test-app.secret` | clé d'essai principale, au format Tauri | généré |
| `keys/other-app.pub`, `keys/other-app.secret` | seconde clé, pour le cas « mauvaise clé » | généré |
| `keys/test-app-crlf.pub` | la clé principale, en fins de ligne Windows (acceptée) | généré |
| `keys/malformed-*.pub` | textes de clé refusés : décalé par une ligne vide, sans commentaire non signé | généré |
| `files/payload.bin` | 65 537 octets déterministes (l'octet `i` vaut `i % 251`) | généré |
| `files/payload-tampered.bin` | le même fichier, avec un bit inversé | généré |
| `files/*.sig` | signatures valides, en fins de ligne Windows, et volontairement fautives | généré |
| `tauri/` | clés et signatures produites par le vrai `tauri signer` 2.12.4 (voir `tauri/README.md`) | Tauri |
| `stack/manifests.json` | lecture de `stack.json`, celui du projet et celui d'une archive | écrit à la main |
| `stack/archives.json`, `stack/archives/` | archives `stack.tar.gz` valides et piégées (liens, `..`, doublons, tailles, 1000 et 1001 entrées…) | généré |
| `stack/demo-1.0.0-aarch64.stack.tar.gz` et son `.sig` | archive publiée de « demo », signée par la clé d'essai | généré |
| `stack/compose.json` | analyse du compose résolu (`cases`) : règles, champs admis, permissions et libellés ; et contrôle du `compose.yaml` d'une archive, tel qu'il est écrit (`archived`) | écrit à la main |
| `stack/compose/` | sources (`*.yaml`, `*.env`) et sorties réelles de `docker compose config`, par Compose 5.5.0 et 2.29.1 : une stack proche de Home Manager, le compose de l'archive publiée de Home Manager 0.2.0 (`home-manager-0.2.0.yaml`, du JSON précédé de la ligne qu'écrivait l'outil ; `.env` : son `.env.example`), une stack qui demande de tout (`permissions`), une stack ordinaire aux nombreux champs sans danger (`generous`), et ce qui touche d'autres conteneurs (`containers`, Compose 5.5.0 seulement : 2.29 ne connaît pas tous ces champs) | Docker Compose |
| `stack/control.json` | dossier de contrôle des mises à jour validées dans l'app : demandes de l'app (`requests`) et `status.json` (`statuses`) | écrit à la main |
| `stack/oci.json`, `stack/oci/` | manifestes, index et configuration réels d'une image de Home Manager (essai S0), et cas écrits à la main | Docker (BuildKit) |
| `admin/responses.json` | contrat de l'administration : la forme de chaque réponse de `/api/admin`, relevée sur le vrai serveur | généré (voir « Contrat de l'administration ») |

**Résultats attendus**
- **Cas de vérification** (`cases`) : `ok`, `legacy`, `key_id_mismatch`, `invalid`, `missing_version` et `version_mismatch`. Ils correspondent aux erreurs de `am_protocol::signature::SignatureError`, et chaque implémentation doit les distinguer.
- **Listes des codes** : `expected_codes` et `parsing_codes` donnent tous les résultats possibles. Les tests Rust et Swift contrôlent que les cas les couvrent tous, et n'en donnent pas d'autre.
- **Cas de lecture** (`parsing`) : `ok`, avec l'identifiant de clé attendu (`key_id`), ou `malformed`, c'est-à-dire clé ou signature illisible. La règle de découpage est décrite dans `docs/PROTOCOLE.md`, section « Signatures ».

**Clés d'essai uniquement.** Le mot de passe des clés de `keys/`, `appsmarket-fixtures` (champ `password` de `signatures.json`), est public, comme ceux de `tauri/` : ne jamais les utiliser pour une vraie app.

**Régénérer les fichiers générés :**

```bash
cargo run -p am-protocol --example gen_fixtures
```

Les clés et les signatures déjà présentes sont reprises telles quelles, car minisign ajoute de l'aléa à chaque signature. Pour en recréer une, supprimer d'abord son fichier. `versions.json`, `tauri/` et, dans `stack/`, les cas écrits à la main et les objets réels ne sont jamais touchés.

**Cas des stacks** : une erreur attendue est le code de `am_protocol::stack::StackError` (`entry_path`, `image_not_pinned`…). Dans les cas écrits à la main, une chaîne qui commence par `@` est remplacée par le contenu du fichier de `fixtures/` qu'elle désigne (une clé, par exemple).

**Régénérer les sorties de Docker Compose** (`stack/compose/`), depuis ce dossier, pour chaque source `<nom>.yaml` (projet `demo`, ou `home-manager` pour les deux de Home Manager) :

```bash
docker compose -p demo --project-directory /srv/stack --env-file permissions.env -f permissions.yaml --profile '*' config --format json > permissions.compose-5.5.0.json
docker run --rm --entrypoint sh -v "$PWD:/c:ro" docker:24-dind -c "cd /c && docker compose -p demo --project-directory /srv/stack --env-file permissions.env -f permissions.yaml --profile '*' config --format json" > permissions.compose-2.29.1.json
```

Le nom du fichier porte la version de Compose qui l'a produit (`docker compose version --short`).

## Contrat de l'administration

`admin/responses.json` fixe la forme des réponses de l'API d'administration (`docs/API.md`). Le test `contract` du serveur (`crates/am-server/tests/server/contract.rs`) joue un scénario sur le vrai serveur et compare ce qu'il relève à ce fichier ; le faux serveur du web le rejoue de son côté. Aucune valeur n'y figure, donc rien de volatil : ni date, ni identifiant, ni jeton.

**Format** : `{"format": 1, "exchanges": [...]}`. Les échanges sont triés par route, méthode, statut et code d'erreur ; plusieurs appels qui donnent la même réponse n'en font qu'un, aux squelettes fusionnés.

| Champ | Contenu |
|---|---|
| `method` | méthode HTTP |
| `route` | motif de route, sans paramètres de requête : `/api/admin/apps/{app}/releases/{version}` |
| `status` | statut HTTP |
| `error` | code d'erreur du corps (`not_found`, `conflict`…), `null` pour une réussite |
| `content_type` | type du corps sans ses paramètres (`application/json`, `image/png`), `null` sans corps |
| `body` | squelette du corps JSON ; `null` sans corps, ou pour un corps qui n'est pas du JSON |
| `server_only` | `true` pour un échange que seul le vrai serveur sait produire (un refus de signature) : le rejeu du web le saute. Absent sinon |

**Squelette** d'une valeur JSON :
- une valeur simple donne `"null"`, `"boolean"`, `"number"` ou `"string"` ;
- un objet donne un objet aux clés triées, chaque valeur remplacée par son squelette ;
- un tableau donne `[]` s'il est vide, sinon `[s]`, où `s` est le squelette fusionné de ses éléments ;
- plusieurs formes à la même place donnent `{"|": [...]}`, dans cet ordre : `"null"`, `"boolean"`, `"number"`, `"string"`, tableau, objet. Un champ tantôt `null`, tantôt renseigné, donne donc `{"|": ["null", "string"]}` ;
- une clé absente de certains des objets fusionnés porte un `?` final : `"images?"`.

Deux objets à la même place fusionnent clé par clé, et deux tableaux fusionnent leur élément : ils ne font jamais deux formes d'un `|`.

```json
{
  "method": "GET",
  "route": "/api/admin/auth/session",
  "status": 200,
  "error": null,
  "content_type": "application/json",
  "body": {
    "csrf_token": "string",
    "stage": "string",
    "sudo_until": { "|": ["null", "string"] },
    "user": { "|": ["null", { "email": "string", "id": "number", "name": "string", "role": "string", "totp_enabled": "boolean" }] }
  }
}
```

**Réécrire le fichier**, après un changement voulu de l'API (`docs/API.md` d'abord) :

```bash
AM_UPDATE_FIXTURES=1 cargo test -p am-server --test server contract
```

Sans `AM_UPDATE_FIXTURES`, le test échoue dès que le serveur ne répond plus comme le fichier, et nomme les échanges qui diffèrent. Le faux serveur du web est alors à corriger aussi.
