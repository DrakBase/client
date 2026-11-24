# API d'administration et de publication

Contrat entre le serveur, l'interface web et l'outil `drakbase`. Les routes publiques (vérification, téléchargement, catalogue) sont dans [PROTOCOLE.md](PROTOCOLE.md). Les choix de sécurité sont expliqués dans [PLAN.md](PLAN.md).

La dernière section, « Stacks Docker », décrit les apps de type `stack` (stacks Docker) et `cli` (exécutables Linux). Elle complète les sections précédentes sans rien y changer pour les apps Swift et Tauri.

## Conventions

- Deux familles de routes :
  - `/api/admin/…` : session par cookie, pour l'interface web ;
  - `/api/v1/publish/…` : jeton `Authorization: Bearer amk_…`, pour l'outil.
- Corps en JSON UTF-8. Seuls les envois d'artefacts, d'icônes et de captures d'écran ont un corps brut.
- Une requête qui modifie et n'a rien à transmettre envoie `{}`, `DELETE` compris : le serveur l'accepte et l'ignore.
- Dates en RFC 3339 UTC.
- La forme de chaque réponse de l'administration est fixée par `fixtures/admin/responses.json`, que relève un test du serveur et que rejoue le faux serveur du web (`fixtures/README.md`, « Contrat de l'administration »). Un changement de cette page s'y reporte.
- **Erreurs** : statut HTTP, plus `{"error": "<code>", "message": "<texte en français>"}`.

| Code | Statut | Sens |
|---|---|---|
| `invalid_request` | 400 | champ ou en-tête manquant ou invalide |
| `version_invalid` | 400 | version illisible ou non canonique |
| `unauthorized` | 401 | pas de session ou de jeton valide, ou lien d'invitation invalide, expiré ou déjà utilisé |
| `invalid_credentials` | 401 | email ou mot de passe faux (message identique dans les deux cas) |
| `mfa_required` | 401 | session en attente du code TOTP |
| `invalid_code` | 401 | code TOTP ou code de secours faux |
| `forbidden` | 403 | rôle, app ou portée du jeton insuffisants |
| `reauth_required` | 403 | action sensible : repasser par `/auth/reauth` |
| `csrf_failed` | 403 | jeton CSRF ou origine absents ou faux |
| `not_found` | 404 | ressource inconnue ou invisible pour cet utilisateur |
| `method_not_allowed` | 405 | méthode non prévue sur cette route |
| `conflict` | 409 | état incompatible (version déjà publiée, dernier superadmin…) |
| `gone` | 410 | version retirée |
| `payload_too_large` | 413 | corps trop gros |
| `unsupported_media_type` | 415 | corps qui n'est pas du JSON |
| `signature_invalid` | 422 | signature minisign refusée |
| `artifact_invalid` | 422 | contenu incohérent (identifiant, version, arch, type de fichier, SHA-256) |
| `rate_limited` | 429 | trop de tentatives, par IP ou par email, ou compte verrouillé après 10 échecs de suite ; `Retry-After` donne l'attente en secondes |
| `internal` | 500 | erreur du serveur, détaillée seulement dans ses journaux |
| `unavailable` | 503 | serveur pas prêt, par exemple base indisponible |

## Sécurité des routes d'administration

- **Cookie de session** : `__Host-am_session` (`Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`).
- **Requêtes qui modifient** (tout sauf `GET` et `HEAD`), toutes trois exigées :
  - `X-CSRF-Token`, avec la valeur donnée par la connexion ou par `GET /api/admin/auth/session` ;
  - un `Origin` identique à celui du serveur, ou `Sec-Fetch-Site: same-origin` ;
  - `Content-Type: application/json`, sauf pour les corps bruts.

  Seules exceptions au jeton CSRF : `POST /auth/login` et `POST /auth/setup`, faites sans session. Le contrôle d'origine et le JSON restent exigés.
- **Pas de CORS.**
- **Étapes d'une session** :
  - `mfa_pending` : mot de passe validé, code attendu ;
  - `mfa_enroll` : compte sans TOTP alors qu'il lui est obligatoire, qui doit l'activer. C'est le cas d'un superadmin, et de tout compte, éditeurs compris, quand le réglage « 2FA pour tous » (`require_totp_all`) est activé ;
  - `full` : session complète.

  Une session ouverte par le seul mot de passe (`mfa_enroll` ou `full`) suit l'obligation du moment, dans les deux sens et sans reconnexion : elle passe en `mfa_enroll` dès que le TOTP devient obligatoire pour son compte, et redevient `full` si l'obligation disparaît (réglage coupé, superadmin redevenu éditeur).

  Seules les routes `auth/*` et `me/totp/*` acceptent une session incomplète.
- **Mode sudo** : 10 minutes après `POST /api/admin/auth/reauth`. Il est exigé partout où cette page indique **sudo**.
  - Corps : `{password, code}`. `code` n'est demandé qu'aux comptes qui ont activé le TOTP.
- **Codes TOTP** : six chiffres. Les espaces et les tirets sont ignorés partout où un code est lu : `/auth/mfa`, `/auth/reauth` et `/me/totp/confirm`.
- **Codes de secours** : acceptés là où un code TOTP ouvre ou confirme une session (`/auth/mfa`, `/auth/reauth`), pas pour activer le TOTP. La casse, les tirets et les espaces sont ignorés.

## Objets

```text
Me          { id, email, name, role: "superadmin"|"editor", totp_enabled }
User        { id, email, name, role, totp_enabled, disabled, created_at, last_login_at?, apps: [slug] }
Token       { id, name, prefix, scopes: ["releases:write"|"releases:publish"|"listing:write"], app?: slug,
              created_at, expires_at, last_used_at?, revoked_at? }
Key         { key_id: "<16 hex>", public_key: "<format Tauri>", status: "active"|"next"|"retired", created_at }
App         { slug, name, description, kind: "swift"|"tauri", bundle_id?, listed,
              default_windows_bundle?: "nsis"|"msi", default_linux_bundle?: "appimage"|"deb"|"rpm",
              icon_url?, screenshots: [Screenshot], created_at, archived_at?, keys: [Key],
              members: [{ id, name, email }] }
Screenshot  { id: "<ULID>", url, width, height, size, sha256, caption, created_at }
Release     { version, channel: "stable"|"beta", status: "draft"|"published"|"yanked", notes_md,
              trusted_key_ids: ["<16 hex>"], created_by?: { id, name, email }, created_at,
              published_at?, yanked_at?, yank_reason?, artifacts: [Artifact] }
Artifact    { id: "<ULID>", os, arch, bundle_type, filename, size, sha256, signed_key_id?,
              signed_version?, min_os_version?, url, created_at }
```

- `members` n'est renvoyé qu'aux superadmins.
- `App.icon_url` désigne `GET /api/admin/apps/{app}/icon`, qui sert l'icône que l'app soit listée, cachée ou archivée.
- `App.screenshots` est dans l'ordre d'affichage. `Screenshot.url` désigne `GET /api/admin/apps/{app}/screenshots/{id}`, qui sert l'image dans les mêmes conditions que l'icône.

## Connexion

| Route | Corps | Réponse |
|---|---|---|
| `POST /api/admin/auth/login` | `{email, password}` | `{stage, csrf_token, user?}` et cookie |
| `POST /api/admin/auth/mfa` | `{code}` (TOTP ou code de secours) | `{stage: "full", csrf_token, user}` et cookie ; `csrf_token` reste celui de la connexion |
| `POST /api/admin/auth/setup` | `{token, password}` (lien d'invitation ou de réinitialisation) | `{stage, csrf_token, user?}` et cookie |
| `GET /api/admin/auth/session` | — | `{stage, csrf_token, user?, sudo_until?}` ; 401 `unauthorized` sans session |
| `POST /api/admin/auth/reauth` | `{password, code?}` | `{sudo_until}` |
| `POST /api/admin/auth/logout` | — | 204 |

- **Mot de passe** : 12 à 128 caractères. Le jeton de session change à la connexion, après la 2FA et au changement de mot de passe.
- **Limites de connexion, 2FA, sudo, changement de mot de passe et activation du TOTP** : 20 tentatives par minute et par IP, 5 par minute et par email ; au-delà, 429 `rate_limited`.
- **Verrouillage** : 15 minutes après 10 échecs de suite (mot de passe ou code, à la connexion, au sudo, au changement de mot de passe ou à l'activation du TOTP), réponse 429 `rate_limited`, avec `Retry-After`.
  - Deux échecs sont « de suite » quand moins de 15 minutes les séparent. Un échec qui suit le précédent de 15 minutes ou plus recommence le compte à 1.
  - Un mot de passe accepté ne remet le compteur d'échecs à zéro que s'il ne reste pas de code à saisir. Sinon, c'est le code accepté qui le fait : des codes faux répétés d'une connexion à l'autre finissent par verrouiller le compte.
  - Un email inconnu suit les mêmes règles, pour ne pas trahir l'existence d'un compte : même compte des échecs, même oubli après 15 minutes, même verrouillage, même réponse.
  - Il reste des écarts, que le serveur ne ferme pas :
    - **redémarrage** : les échecs d'un compte sont comptés en base, ceux d'un email inconnu en mémoire. Un redémarrage du serveur oublie les seconds, pas les premiers ;
    - **succès du titulaire** : une connexion réussie remet le compteur d'un compte à zéro, et rien ne remet celui d'un email inconnu, dont personne n'a le mot de passe. Après neuf échecs, le dixième verrouille toujours un email inconnu ; il ne verrouille pas un compte dont le titulaire s'est connecté entre-temps ;
    - **table pleine** : le serveur suit 10 000 emails inconnus au plus à la fois. Tant qu'aucune place ne se libère, un email inconnu de plus n'est ni compté, ni verrouillé, ni journalisé pour lui-même ; un compte l'est toujours ;
    - **délai** : l'échec d'un compte écrit en base, son compteur et sa ligne du journal. Celui d'un email inconnu passe par le même écrivain, dans une transaction qui n'écrit rien quand sa ligne du journal n'est pas due : les deux réponses attendent leur tour de la même façon, et il reste le temps de l'écriture elle-même.
- **Durée des sessions** : 8 h sans activité, 7 jours au plus. Une session en `mfa_pending` dure 5 minutes et permet 5 essais.
- **Code faux** : `POST /auth/mfa` répond 401 `invalid_code`. Au 5ᵉ échec, la session est supprimée, et la réponse est 401 `unauthorized` : il faut se reconnecter.
- **Compte désactivé** : `POST /auth/login` répond 403 `forbidden`, une fois le bon mot de passe donné seulement ; un mot de passe faux donne 401 `invalid_credentials`, comme pour tout compte.
- **Liens** : une invitation ou une réinitialisation a la forme `<AM_PUBLIC_URL>/admin/setup#token=<jeton>`, valable 72 h et une seule fois. Le jeton reste dans le fragment de l'adresse et ne part donc pas dans les journaux. Un lien invalide, expiré ou déjà utilisé donne 401 `unauthorized` ; le lien valide d'un compte désactivé, 403 `forbidden`.

## Mon compte

| Route | Contraintes | Réponse |
|---|---|---|
| `POST /api/admin/me/totp/setup` | session `mfa_enroll` ou `full` ; 409 si le TOTP est déjà actif ; sudo en session `full` | `{secret, otpauth_uri, qr_data_url}`, où `qr_data_url` est une image `data:image/svg+xml;base64,…` à mettre dans un `<img>` (la CSP permet `img-src data:`) |
| `POST /api/admin/me/totp/confirm` `{code}` | après `setup` ; sans sudo ; limites et verrouillage de la connexion (429 `rate_limited`) ; cinq codes faux effacent le secret (409 `conflict`) | `{recovery_codes: [10]}` ; la session passe en `full` |
| `POST /api/admin/me/totp/disable` | sudo ; refusé aux superadmins | 204 |
| `POST /api/admin/me/password` `{current_password, new_password}` | sudo ; le mot de passe actuel est vérifié avec les limites et le verrouillage de la connexion (429 `rate_limited`) | 204 ; les autres sessions sont fermées, et les liens de réinitialisation du compte encore valables sont supprimés |
| `POST /api/admin/me/recovery-codes` | sudo | `{recovery_codes: [10]}` ; les anciens codes sont révoqués |
| `GET /api/admin/me/tokens` | — | `[Token]` |
| `POST /api/admin/me/tokens` `{name, scopes, app?, expires_in_days?}` | sudo ; de 1 à 3650 jours, 365 par défaut | 201 `{token: Token, secret: "amk_…"}` ; le secret n'est montré qu'une fois |
| `DELETE /api/admin/me/tokens/{id}` | — | 204 |

- **Activer le TOTP** : dans une session `full`, c'est une action sensible. Sans sudo, `POST /me/totp/setup` répond 403 `reauth_required` ; un TOTP déjà actif donne 409 `conflict`, avec ou sans sudo.
  - Une session `mfa_enroll` n'a pas de sudo à ouvrir (`/auth/reauth` exige une session `full`) : `setup` ne lui en demande pas.
  - `confirm` ne demande jamais le sudo : une activation commencée en sudo se termine même s'il a expiré entre-temps. Un secret peut rester en attente longtemps, et une session ne suffit pas à le lire : son code se vérifie donc comme celui de `/auth/mfa`.
    - Mêmes limites par IP et par email, même verrouillage du compte : 429 `rate_limited`, avant toute vérification.
    - Un code faux donne 401 `invalid_code`. Il compte parmi les échecs de suite du compte, et ajoute une ligne `me.totp_failed` au journal.
    - Le cinquième code faux donné pour un même secret l'efface : la réponse est 409 `conflict`, et il faut reprendre à `setup`, qui redemande le sudo en session `full`. Chaque `setup` donne un nouveau secret, et cinq nouveaux essais.
    - Sans secret en attente, ou si le secret a changé pendant la confirmation, la réponse est aussi 409 `conflict`.
    - Un code accepté remet le compteur d'échecs du compte à zéro, comme à `/auth/mfa`.

## Utilisateurs (superadmin)

| Route | Contraintes | Réponse |
|---|---|---|
| `GET /api/admin/users` | — | `[User]` |
| `POST /api/admin/users` `{email, name, role}` | sudo | 201 `{user, invitation_url}`, lien valable 72 h |
| `PATCH /api/admin/users/{id}` `{name?, role?, disabled?}` | sudo ; le dernier superadmin actif reste superadmin | `User` |
| `DELETE /api/admin/users/{id}` | sudo ; ni soi-même, ni le dernier superadmin | 204 ; ses lignes du journal restent, avec leur libellé |
| `POST /api/admin/users/{id}/reset-totp` | sudo | 204 |
| `POST /api/admin/users/{id}/reset-link` | sudo | `{reset_url}`, lien valable 72 h |

## Apps

| Route | Qui | Réponse |
|---|---|---|
| `GET /api/admin/apps` | un superadmin voit toutes les apps, archivées comprises (`archived_at` renseigné) ; un éditeur ne voit que ses apps non archivées | `[App]` |
| `POST /api/admin/apps` `{slug, name, kind, bundle_id?, description?}` | superadmin | 201 `App` |
| `GET /api/admin/apps/{app}` | membres et superadmins | `App` |
| `PATCH /api/admin/apps/{app}` | éditeur : `name`, `description`, `listed`, types par défaut ; superadmin : tout, dont `bundle_id` | `App` |
| `GET /api/admin/apps/{app}/icon` | membres et superadmins | `image/png` ; 404 sans icône |
| `PUT /api/admin/apps/{app}/icon` (corps `image/png`, au plus 512 Kio) | éditeur | 204 |
| `DELETE /api/admin/apps/{app}/icon` | éditeur | 204 |
| `GET /api/admin/apps/{app}/screenshots/{id}` | membres et superadmins | `image/png` |
| `POST /api/admin/apps/{app}/screenshots` (corps `image/png`) | éditeur ; voir « Captures d'écran » | 201 `Screenshot` ; 200 et la capture existante si l'image est déjà là |
| `PATCH /api/admin/apps/{app}/screenshots/{id}` `{caption}` | éditeur | `Screenshot` |
| `PUT /api/admin/apps/{app}/screenshots/order` `{ids}` | éditeur ; `ids` : toutes les captures de l'app, chacune une fois | `[Screenshot]` |
| `DELETE /api/admin/apps/{app}/screenshots/{id}` | éditeur | 204 |
| `DELETE /api/admin/apps/{app}` | superadmin, sudo ; archive l'app, sans rien effacer | 204 |
| `POST /api/admin/apps/{app}/unarchive` | superadmin, sudo | `App` |
| `PUT /api/admin/apps/{app}/members` `{user_ids}` | superadmin, sudo | `App` |
| `POST /api/admin/apps/{app}/keys` `{public_key, status: "active"\|"next"}` | superadmin, sudo | 201 `Key` |
| `PATCH /api/admin/apps/{app}/keys/{key_id}` `{status}` | superadmin, sudo | `Key` |

- Le slug d'une app est immuable.
- « éditeur » veut dire un éditeur de l'app ou un superadmin.
- **Icône** : PNG seulement, 512 Kio au plus. Sa signature et son en-tête `IHDR` sont contrôlés comme ceux d'une capture d'écran, sans limite de dimensions. Les erreurs sont celles des captures : 415, 400 et 413.

### Captures d'écran

La fiche publique d'une app montre sa description et ses captures d'écran, dans l'ordre choisi.

- **Image** : PNG seulement (signature et en-tête `IHDR` lus avant d'écrire quoi que ce soit), 8 Mio au plus, de 320 à 8192 pixels de côté. Le serveur ne la retouche pas.
- **Nombre** : 10 captures par app au plus ; au-delà, 409 `conflict`.
- **Même image** : une capture dont le SHA-256 est déjà celui d'une capture de l'app n'est pas ajoutée une seconde fois ; la réponse est 200, avec la capture existante. Un envoi interrompu se reprend donc sans doublon.
- **Ordre** : une nouvelle capture va à la fin. `PUT …/screenshots/order` donne l'ordre complet.
- **Légende** : texte brut, 200 caractères au plus, vide par défaut. Elle sert de texte de remplacement à l'image.
- **Retrait** : la capture et son fichier sont supprimés ; l'ordre des autres se resserre.
- **Erreurs**, comme pour l'icône : `Content-Type` autre que `image/png`, 415 `unsupported_media_type` ; corps qui n'est pas un PNG lisible, ou dimensions hors limites, 400 `invalid_request` ; corps trop gros, 413 `payload_too_large`, avant d'en lire plus que la limite.
- Une seule clé est active. En activer une autre retire automatiquement la précédente (`retired`), sans erreur.

## Versions et artefacts

Toutes ces routes sont réservées aux éditeurs de l'app et aux superadmins.

| Route | Contraintes | Réponse |
|---|---|---|
| `GET /api/admin/apps/{app}/releases` | toutes les versions, quel que soit leur état, de la plus haute à la plus basse | `[Release]` |
| `POST /api/admin/apps/{app}/releases` `{version, channel?, notes_md?, trusted_key_ids?}` | version canonique, sans `+build` ; sans `channel`, le canal `stable` ; sans `notes_md`, des notes vides | 201 `Release` (brouillon) |
| `GET /api/admin/apps/{app}/releases/{version}` | — | `Release` |
| `PATCH /api/admin/apps/{app}/releases/{version}` `{notes_md?, channel?, trusted_key_ids?}` | `trusted_key_ids` seulement en brouillon | `Release` |
| `POST …/releases/{version}/publish` | au moins un artefact de mise à jour | `Release` |
| `POST …/releases/{version}/yank` `{reason?}` | version publiée | `Release` |
| `POST …/releases/{version}/unyank` | version retirée | `Release` |
| `DELETE …/releases/{version}` | brouillon (éditeur) ou version retirée (superadmin) | 204 |
| `PUT …/releases/{version}/artifacts/{os}/{arch}/{bundle_type}` | brouillon ; corps brut, voir « Envoi d'un artefact » | 201 `Artifact` |
| `DELETE /api/admin/apps/{app}/artifacts/{artifact_id}` | brouillon, sauf pour un superadmin | 204 |

- **Création** : `channel`, `notes_md` et `trusted_key_ids` sont facultatifs, et `null` vaut absent. Une version qui existe déjà, brouillon compris, donne 409 `conflict`, sans rien changer.
- **Passer une bêta en stable** : `PATCH` avec `{channel: "stable"}`. Le canal ne fait pas partie de la signature.
- **`trusted_key_ids`** : les clés que ce build embarque, en 16 caractères hexadécimaux (casse indifférente, doublons retirés).
  - Une clé inconnue du serveur est acceptée, par exemple la clé de secours d'une app Swift.
  - Une liste vide veut dire « non déclarées » : le serveur ne filtre alors pas les mises à jour par clé pour les installations de cette version (voir PROTOCOLE.md).
  - L'outil les déclare lui-même, selon le `kind` de l'app : pour une app Swift, les clés de `DrakBasePublicKeys` (ou de l'ancienne `AppsMarketPublicKeys`) dans l'Info.plist ; pour une app Tauri, la clé `plugins.updater.pubkey` de `tauri.conf.json`.

## Statistiques, journal et réglages

| Route | Qui | Réponse |
|---|---|---|
| `GET /api/admin/stats?from=AAAA-MM-JJ&to=AAAA-MM-JJ` | tous, sur les apps visibles | `{from, to, apps: [{slug, name, active_installs, checks, downloads}]}` |
| `GET /api/admin/apps/{app}/stats?from&to&by=version\|platform\|channel` | membres et superadmins | `{installs: [{day, key, installs, checks}], downloads: [{day, key, source, downloads}]}` |
| `GET /api/admin/audit?app=&before=&limit=` | superadmin (tout), éditeur (ses apps) | `{entries: [{id, at, actor, action, app?, target?, details, ip?}], next_before?}` |
| `GET /api/admin/settings` | superadmin | `{require_totp_all, public_title}` |
| `PATCH /api/admin/settings` | superadmin, sudo | réglages |

**Statistiques :**
- `active_installs` : installations distinctes du dernier jour complet de la période. `checks` et `downloads` : totaux de la période.
- `day` est au format `AAAA-MM-JJ` (UTC). `key` dépend de `by` :
  - `version` : `1.2.0` ;
  - `platform` : `<os>-<arch>-<bundle_type>`, par exemple `darwin-universal-app` ;
  - `channel` : `stable` ou `beta`. Pour les installations, c'est le canal que suit l'installation. Pour les téléchargements, qui n'enregistrent pas de canal, c'est le canal actuel de la version ; si elle a été supprimée, `beta` pour une pré-version et `stable` sinon.

**Codes `action` du journal :**

| Domaine | Codes |
|---|---|
| connexion | `auth.login`, `auth.login_failed`, `auth.locked`, `auth.mfa`, `auth.mfa_failed`, `auth.reauth`, `auth.reauth_failed`, `auth.setup`, `auth.logout` |
| mon compte | `me.password`, `me.password_failed`, `me.recovery_codes`, `me.totp_enabled`, `me.totp_failed`, `me.totp_disabled` |
| utilisateurs | `user.invite`, `user.reset_link`, `user.update`, `user.delete`, `user.reset_totp`, `user.create`, `user.set_password` |
| apps et clés | `app.create`, `app.update`, `app.icon`, `app.screenshot`, `app.members`, `app.archive`, `app.unarchive`, `key.add`, `key.update` |
| versions | `release.create`, `release.update`, `release.publish`, `release.yank`, `release.unyank`, `release.delete`, `artifact.upload`, `artifact.delete`, `manifest.upload` |
| jetons et réglages | `token.create`, `token.revoke`, `settings.update` |

- `user.invite` : compte créé dans l'administration, avec son lien d'invitation. `user.reset_link` : lien de réinitialisation créé.
- `manifest.upload` : manifeste ou index d'image d'une stack Docker envoyé (section « Stacks Docker ») ; `details` : `{digest, media_type, platform?}`.
- `app.screenshot` : capture ajoutée, retirée, légendée ou déplacée ; `details` dit laquelle et quoi (`{op: "add"|"delete"|"caption"|"order", id?}`). La description passe par `app.update`.
- `user.create` et `user.set_password` ne viennent que des commandes du serveur (`create-superadmin`, `set-password`), comme `user.reset_totp` quand `reset-totp` le lance. Leur auteur est alors `cli`.
- `me.totp_failed` : code faux à `/me/totp/confirm` ; `details` : `{attempt}`, le rang du code faux pour ce secret (au cinquième, le secret est effacé).
- `auth.login_failed` : `details.reason` vaut `password`, `disabled`, `unknown_email` ou `unknown_emails_skipped`.
  - `unknown_email` : seul le premier échec d'une suite est journalisé, soit une ligne au plus par email et par quart d'heure, et cent lignes au plus par quart d'heure, tous emails inconnus confondus. Le journal ne grossit donc pas au rythme des tentatives ; les échecs d'un compte, que le verrouillage borne, sont tous journalisés.
  - `unknown_emails_skipped` : une fois ce plafond atteint, ou quand le serveur suit déjà 10 000 emails inconnus, les tentatives du quart d'heure sur des emails inconnus ne sont plus notées une à une, mais comptées. Le quart d'heure passé, une ligne les résume : `details.attempts` donne leur nombre, `details.since` le début du quart d'heure (RFC 3339), et son auteur est `emails inconnus`. Elle est écrite à la tentative suivante sur un email inconnu, ou dans la minute par le serveur.

Une interface qui rencontre un code inconnu l'affiche tel quel.

## Publication par l'outil (jeton)

- Le jeton s'envoie dans l'en-tête `Authorization: Bearer amk_…`.
- **Portées** :
  - `releases:write` permet de créer une version, d'envoyer des artefacts et de modifier une version encore en brouillon. Elle permet aussi de corriger les notes d'une version déjà publiée ou retirée, mais pas son canal ;
  - `releases:publish` permet de publier, de retirer, et de changer le canal d'une version déjà publiée, puisque ce changement l'ouvre à d'autres utilisateurs ;
  - `listing:write` permet de modifier la fiche publique de l'app : sa description et ses captures d'écran. Un jeton volé avec cette portée peut changer la vitrine, pas livrer de code.
- **Droits effectifs** : la portée du jeton, croisée avec le rôle et les apps actuels de son propriétaire, et avec l'app du jeton s'il en a une.

| Route | Portée | Réponse |
|---|---|---|
| `GET /api/v1/publish/whoami` | toutes | `{user: {email, name, role}, token: {name, scopes, app?, expires_at}, max_artifact_bytes, public_url}` |
| `GET /api/v1/publish/apps/{app}` | toutes | `{slug, name, kind, bundle_id?, description, screenshots: [Screenshot], keys: [Key], latest: {stable?, beta?}}` |
| `PATCH /api/v1/publish/apps/{app}` `{description}` | `listing:write` | la même réponse que `GET` |
| `POST /api/v1/publish/apps/{app}/screenshots` (corps `image/png`) | `listing:write` | 201 `Screenshot` ; 200 et la capture existante si l'image est déjà là |
| `PATCH /api/v1/publish/apps/{app}/screenshots/{id}` `{caption}` | `listing:write` | `Screenshot` |
| `PUT /api/v1/publish/apps/{app}/screenshots/order` `{ids}` | `listing:write` | `[Screenshot]` |
| `DELETE /api/v1/publish/apps/{app}/screenshots/{id}` | `listing:write` | 204 |
| `POST /api/v1/publish/apps/{app}/releases` `{version, channel?, notes_md?, trusted_key_ids?}` | `releases:write` | 201 `Release` créée ; 200 si un brouillon de cette version existe ; 409 si la version existe hors brouillon |
| `PUT /api/v1/publish/apps/{app}/releases/{version}/artifacts/{os}/{arch}/{bundle_type}` | `releases:write` | 201 `Artifact` |
| `DELETE /api/v1/publish/apps/{app}/artifacts/{artifact_id}` | `releases:write` ; brouillon seulement | 204 |
| `PATCH /api/v1/publish/apps/{app}/releases/{version}` `{notes_md?, channel?, trusted_key_ids?}` | `releases:write`, plus `releases:publish` pour le canal d'une version publiée ; `trusted_key_ids` seulement en brouillon (409 `conflict` sinon) | `Release` |
| `POST /api/v1/publish/apps/{app}/releases/{version}/publish` `{}` | `releases:publish` | `Release` |
| `POST /api/v1/publish/apps/{app}/releases/{version}/yank` `{reason?}` | `releases:publish` | `Release` |

- `public_url` est `AM_PUBLIC_URL`, sans barre finale. C'est l'adresse que voient les machines : l'outil en tire l'hôte des noms d'images d'une stack et la commande d'installation, même quand il publie par une autre adresse (un tunnel, une adresse interne).
- `latest.stable` et `latest.beta` sont des chaînes : la dernière version publiée de chaque canal, absente s'il n'y en a pas.
- **Fiche** : `description` est du texte brut, 4000 caractères au plus, les paragraphes séparés par une ligne vide. Les captures suivent les règles de « Captures d'écran » ; `Screenshot.url` y désigne l'image de l'administration, que le jeton ne lit pas : l'outil compare les captures par leur `sha256`.
- **Création d'une version** : `channel`, `notes_md` et `trusted_key_ids` sont facultatifs ; `null` vaut absent. Sans eux, un nouveau brouillon est sur le canal `stable`, avec des notes vides et sans clés déclarées.
- **Brouillon existant** (réponse 200) : il prend le canal, les notes et les clés envoyés, puis il est renvoyé avec ses artefacts déjà reçus. C'est ce qui permet à l'outil de reprendre une publication interrompue.
  - Un champ absent garde la valeur du brouillon : relancer une publication sans redonner les notes ne les efface pas.
  - Un champ présent la remplace, même vide : `notes_md: ""` efface les notes, et `trusted_key_ids: []` les clés déclarées.

## Ajout d'une app par l'outil

`drakbase init` ne crée ni app ni clé avec son jeton, et l'API de publication ne le permet pas : un jeton volé permettrait sinon de créer une app avec sa propre clé, et de distribuer du code depuis le serveur. L'outil ouvre donc l'administration sur une page pré-remplie, puis attend.

- **Adresse** : `<AM_PUBLIC_URL>/admin/add-app#slug=…&name=…&kind=swift|tauri|stack|cli&bundle_id=…&public_key=…&editor=…`.
  - Le fragment est encodé comme une chaîne de requête, et la page le lit avec `URLSearchParams`. `name`, `bundle_id` et `editor` peuvent manquer.
  - `public_key` est la clé publique principale au format Tauri. Ses `+`, `/` et `=` sont encodés.
  - `editor` est l'adresse du compte du jeton de l'outil, quand ce compte n'est pas superadmin (`whoami`). Un tel jeton ne voit que les apps dont son compte est éditeur : sans cet ajout, l'outil attendrait une app qu'il ne voit pas.
  - Le fragment ne part pas vers le serveur : il ne finit dans aucun journal. Il survit à la connexion, si la session doit d'abord être ouverte.
- **Page**, réservée aux superadmins. Elle n'agit qu'après un clic, et affiche l'identifiant de la clé, à comparer avec celui du terminal.
  - App absente : `POST /api/admin/apps`, puis `POST /api/admin/apps/{app}/keys` avec `status: "active"`, en mode sudo.
  - App sans clé active, et sans cette clé : `POST …/keys` seul.
  - Dans ces deux cas, avec `editor` : le même clic ajoute ensuite ce compte aux éditeurs, par `PUT …/members` (les éditeurs actuels, plus lui).
  - Cette clé déjà active : rien à faire, sauf ajouter `editor` aux éditeurs s'il n'y est pas (un clic, `PUT …/members` seul).
  - `editor` sans compte de cette adresse : un avertissement, qui renvoie à l'onglet Éditeurs. Un superadmin n'est jamais ajouté : il voit déjà toutes les apps.
  - Une autre clé active, cette clé en `next` ou `retired`, un autre type d'app, une app archivée, ou une clé déjà celle d'une autre app (le serveur la refuserait : une paire de clés par app) : refus expliqué, sans aucune écriture.
- **Outil** : `GET /api/v1/publish/apps/{app}` toutes les 2 s, 15 minutes au plus, jusqu'à trouver la clé avec le statut `active`. `not_found` veut dire que l'app n'est pas encore créée, ou que le compte du jeton n'en est pas éditeur. Les cas que la page refuserait sont signalés par l'outil avant d'ouvrir quoi que ce soit, comme un jeton limité à une autre app (`whoami`), qui ne verrait jamais celle-ci.

## Envoi d'un artefact

Même format pour l'interface web (session) et pour l'outil (jeton).

**En-têtes :**
- `Content-Length` : obligatoire.
- `X-AppsMarket-Filename` : obligatoire. Le serveur le réduit à `[A-Za-z0-9._-]{1,200}`.
- `X-AppsMarket-Signature` : contenu du `.sig` au format Tauri, obligatoire sauf pour `dmg`.
- `X-AppsMarket-SHA256` : facultatif, comparé à l'empreinte calculée.

**Traitement :**
1. Les droits, l'emplacement (OS, arch, type) et la taille sont contrôlés avant de lire le corps. La taille maximale vaut 1 Gio par défaut (`AM_MAX_ARTIFACT_BYTES`).
   - Un envoi refusé à ce stade reçoit sa réponse d'erreur. Le serveur lit ensuite et jette le reste du corps, pendant quelques secondes au plus : sinon la connexion est réinitialisée, et Caddy renvoie 502 à la place de l'erreur (constaté en essai).
2. Le corps est écrit en flux, avec SHA-256 et vérification minisign en une seule passe.
3. La version du commentaire de confiance doit être celle de la version. Tous les artefacts d'une version doivent être signés par la même clé, `active` ou `next`. La signature est enregistrée sans aucun blanc autour.
4. Le contenu est inspecté :
   - `app` : `CFBundleIdentifier` doit être égal au `bundle_id` de l'app, `CFBundleShortVersionString` normalisé à la version, et les architectures du binaire à l'`arch` déclarée ; `LSMinimumSystemVersion` est relevé ;
   - Windows et Linux : type de fichier et architecture du binaire. Les limites de cette inspection sont dans PLAN.md, « Risques et limites ».
5. Un emplacement ne peut être remplacé que tant que la version est un brouillon.

**Erreurs :**
- en-tête absent ou mal formé, `Content-Length` compris : 400 `invalid_request` ;
- version qui n'est plus un brouillon : 409 `conflict` ;
- corps trop gros : 413 `payload_too_large` ;
- signature illisible, invalide, faite avec une autre clé que celles permises, ou version signée différente : 422 `signature_invalid` ;
- contenu incohérent (identifiant, version, arch, type de fichier) ou SHA-256 différent de `X-AppsMarket-SHA256` : 422 `artifact_invalid`.

## Stacks Docker

Les choix sont expliqués dans [PLAN.md](PLAN.md), « Stacks Docker ». Les formats (`stack.tar.gz`, `stack.json`, registre `/v2/`) sont dans [PROTOCOLE.md](PROTOCOLE.md), à la section du même nom. Construit en vague S (serveur S2, outil S3).

**Objets :**
- `App.kind` vaut `swift`, `tauri`, `stack` (stack Docker) ou `cli` (exécutable Linux, comme l'agent `drakbase-stack`).
  - Une app `stack` ou `cli` n'a ni `bundle_id` ni types par défaut : en donner un, à la création comme par `PATCH`, donne 400 `invalid_request`.
  - Une app qui a déjà des versions ne change pas de famille (Swift ou Tauri, stack, cli) : 409 `conflict`.
- `Artifact.bundle_type` prend aussi `stack` et `bin`, sur `linux` seulement, sans arch `universal`.
- `Release` d'une app `stack` porte en plus `images: [{image, digest, platforms: [{platform, size}]}]`, relevé à l'envoi de l'archive. Il vaut `[]` avant la première archive. `size` est la taille de la configuration et des couches de cette plateforme ; les `platforms` suivent l'ordre de l'index. Les autres apps n'ont pas ce champ.
- Clé `platform` des statistiques : `linux-aarch64-stack`, `linux-x86_64-bin`, etc.

**Publication des images** : jeton avec la portée `releases:write`, app de type `stack` seulement (sinon 400 `invalid_request`).

| Route | Réponse |
|---|---|
| `HEAD /api/v1/publish/apps/{app}/blobs/{digest}` | 200, avec la taille en `Content-Length`, si le serveur a la couche ; 404 sinon |
| `PUT /api/v1/publish/apps/{app}/blobs/{digest}` (corps brut) | 201 `{digest, size}` ; 200 si elle était déjà là |
| `HEAD /api/v1/publish/apps/{app}/manifests/{digest}` | 200, avec `Content-Type` (le type du manifeste) et `Content-Length` ; 404 sinon |
| `PUT /api/v1/publish/apps/{app}/manifests/{digest}` (corps : le manifeste ; `Content-Type` : son type) | 201 `{digest, media_type, platform?, platforms?}` ; 200 s'il était déjà là |

- **`HEAD`** : la réponse n'a pas de corps. 404 : absent ; 400 : app d'un autre type ou empreinte illisible ; 401 et 403 comme ailleurs.
- **Empreinte** (`digest`) : `sha256:` suivi de 64 caractères hexadécimaux minuscules. Sinon, 400 `invalid_request`.
- **Couche** :
  - `Content-Length` est obligatoire, et vaut au plus `AM_MAX_ARTIFACT_BYTES` ;
  - le corps est écrit en flux dans `tmp/`, avec le calcul du SHA-256 ;
  - il n'est rangé que si son empreinte est celle annoncée, sinon 422 `artifact_invalid` ;
  - le `PUT` d'une couche déjà là lit le corps jusqu'à `Content-Length`, puis répond 200 ; une autre longueur donne 400 ;
  - une couche enregistrée dont le fichier manque, ou n'a pas la taille enregistrée, est oubliée au `HEAD` ou au `PUT` : 404, puis 201 au renvoi.
- **Manifeste** : 2 Mio au plus, de l'un de ces types :
  - `application/vnd.oci.image.index.v1+json` ;
  - `application/vnd.oci.image.manifest.v1+json` ;
  - `application/vnd.docker.distribution.manifest.list.v2+json` ;
  - `application/vnd.docker.distribution.manifest.v2+json`.

  Un autre type donne 415 `unsupported_media_type`. Le SHA-256 du corps doit être l'empreinte annoncée, et son `mediaType`, s'il en a un, le `Content-Type`. Sinon, ou si un des points suivants n'est pas respecté, la réponse est 422 `artifact_invalid`, avec un message qui nomme ce qui manque :
  - **manifeste d'image** :
    - sa configuration et toutes ses couches sont déjà sur le serveur, chacune de la taille de son descripteur ; la configuration fait 4 Mio au plus ;
    - la plateforme (`platform`) est lue dans la configuration (`os`, `architecture`, `variant`), et doit être de la table de PROTOCOLE.md ;
    - les couches sont de type `application/vnd.oci.image.layer.v1.tar`, `…tar+gzip` ou `…tar+zstd`, ou `application/vnd.docker.image.rootfs.diff.tar.gzip` ;
    - les couches qui renvoient ailleurs (`urls`), ou à distribution restreinte, sont refusées ;
  - **index** :
    - tous ses manifestes sont déjà sur le serveur, et ce sont des manifestes d'image de la taille de leur descripteur (`platforms` les liste) ;
    - chacun a une plateforme de la table, une seule fois, et sa configuration a la plateforme que l'index lui donne. Les manifestes d'attestation (plateforme `unknown/unknown`) sont refusés : l'outil construit sans (`--provenance=false --sbom=false`).

  La lecture des manifestes, des index et des configurations est dans `am_protocol::stack::oci`, avec des cas tirés de vraies images dans `fixtures/stack/oci.json`.
- **Rattachement** : ni une couche ni un manifeste n'appartient à une version. Ce qu'aucune version ne désigne est effacé après 24 h. Un `HEAD` qui trouve un objet, comme le `PUT` d'un objet déjà là, remet ce délai à zéro : une couche que l'outil a vue sur le serveur, et qu'il n'envoie donc pas, ne disparaît pas avant l'envoi de l'archive qui la désigne.
- **Journal** : chaque manifeste ajoute une ligne `manifest.upload` (`target` : l'empreinte), mais pas chaque couche.

**Envoi d'une archive `stack`** : `PUT …/releases/{version}/artifacts/linux/{arch}/stack`, avec les en-têtes et le traitement de « Envoi d'un artefact ».
- **Emplacement** : `stack` et `bin` sont réservés aux apps `stack` et `cli`, qui n'ont qu'eux ; l'OS est `linux` et l'arch autre que `universal`. Tout autre couple donne 400 `invalid_request`, avant de lire le corps.
- **Taille** : 17 Mio au plus (16 Mio de tar et 1 Mio de marge pour gzip). Un `Content-Length` plus grand donne 413 `payload_too_large`, avant de lire le corps.
- **Contenu** (sinon 422 `artifact_invalid`) :
  - l'archive et `stack.json` suivent PROTOCOLE.md ;
  - `app` est le slug de l'app, et `version` la version publiée ;
  - chaque image de `images` est sur le serveur, et disponible pour l'arch de l'emplacement : soit un index qui contient cette plateforme, soit un manifeste de cette plateforme ;
  - les `images` sont les mêmes que celles des autres archives `stack` de la version, l'emplacement remplacé excepté. S'il n'en reste aucune, les images de la nouvelle archive remplacent celles de la version.
- **Clés** : les identifiants des `public_keys` deviennent les `trusted_key_ids` de la version. S'ils étaient déjà déclarés et diffèrent (comparés comme des ensembles, ordre et casse indifférents), la réponse est 422 `artifact_invalid`.
- **Version de Docker Compose** : `min_compose_version` devient le `min_os_version` de l'artefact.
- **Images** : elles sont enregistrées pour la version (`Release.images`), et le registre les sert dès qu'elle est publiée. Supprimer la dernière archive `stack` d'une version efface ses images.
- **Journal** : la ligne `artifact.upload` d'une archive `stack` porte en plus `images`, et `trusted_key_ids` quand la version les reçoit.

**Envoi d'un exécutable `bin`** :
- une app `cli`, l'OS `linux`, et un ELF de type `ET_EXEC`, ou `ET_DYN` (exécutable indépendant de la position), de l'arch déclarée. La machine et la classe (32 ou 64 bits) de l'en-tête vont ensemble : un x86-64 en 32 bits n'est pas `x86_64`. La même lecture vaut pour une AppImage ;
- l'outil lit les `trusted_key_ids` dans `drakbase.json` (`public_keys`), à la racine du projet ou par `--drakbase-json`.

**Publication** : une archive `stack` ou un exécutable `bin` compte comme artefact de mise à jour.

**Ajout d'une app par l'outil** : `kind` prend aussi `stack` et `cli`, toujours sans `bundle_id`.

**Journal** : le code `manifest.upload` est dans le domaine « versions », avec `details` : `{digest, media_type, platform?}`. Le ménage des couches n'est pas journalisé, pas plus que celui des envois temporaires.

**Caddy** :
- `PUT /api/v1/publish/apps/*/blobs/*` accepte jusqu'à `AM_MAX_ARTIFACT_BYTES` ;
- les manifestes restent sous la limite générale de 2 Mio ;
- `/v2/*` et `/install/*` ne sont jamais compressés, comme `/dl/*`.
