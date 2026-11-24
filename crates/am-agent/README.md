# drakbase-stack

Agent Linux qui installe et met à jour les stacks Docker publiées sur DrakBase - Apps Market (jalon S4 de [docs/PLAN.md](../../docs/PLAN.md), « Stacks Docker »). Le contrat est dans [docs/PROTOCOLE.md](../../docs/PROTOCOLE.md), sections « Stacks Docker » et « Agent `drakbase-stack` ». Ce qu'il partage avec le serveur et l'outil (signatures, archive, `stack.json`, analyse du compose) vient de `am_protocol`.

C'est un exécutable statique (musl) pour aarch64 et x86_64. Il tourne en root, comme tout ce qui pilote Docker.

## Commandes

| Commande | Effet |
|---|---|
| `install <app> --key <clé> [--server <adresse>] [--channel beta] [--no-stats] [--accept]` | Première installation. La clé, au format Tauri, est l'ancre de confiance. Les permissions sont demandées dans un terminal, sinon `--accept` |
| `update [<app>] [--accept] [--retry]` | Met à jour une stack, ou toutes, tout de suite. `--accept` accepte les permissions nouvelles ; `--retry` reprend une version écartée |
| `check [<app>]` | Vérifie si une version plus récente existe, la télécharge, contrôle sa signature et son compose, puis la retire : rien n'est installé. Pour une stack qui a un dossier de contrôle, le résultat va dans `control/status.json`, que l'app lit |
| `request <app>` | Traite les demandes que l'interface de l'app a déposées dans `control/request/` (vérifier, installer la version proposée). Lancé par systemd. Refusé (code 1) quand la version qui tourne n'a pas `app_control` |
| `status` | Agent, minuteur, et pour chaque stack : versions (installée, précédente, en échec, écartées), état des services |
| `logs <app> [-f] [-n <lignes>]` | Journal des services de la version qui tourne |
| `rollback <app> [--accept]` | Relance la version précédente et écarte celle qu'il quitte. Après un échec laissé en l'état, relance la dernière version saine. La version relancée est d'abord contrôlée avec le `.env` actuel : une permission nouvelle arrête (code 3) jusqu'à `--accept` |
| `restart <app> [--accept]` | Relance la version installée avec le `.env` actuel, après un réglage changé à la main. Le compose est d'abord contrôlé avec ce `.env` : une permission nouvelle arrête (code 3) jusqu'à `--accept` |
| `uninstall <app> [--delete-data]` | Arrête et retire la stack, ses images et ses fichiers. Les volumes et le `.env` restent, sauf avec `--delete-data`, qui nomme les volumes à supprimer, signale ceux qu'une autre stack peut partager (nom fixe), et demande de taper le nom de l'app dans un terminal |
| `auto on\|off` | Unités systemd : un minuteur quotidien (`RandomizedDelaySec=1h`, `Persistent=true`) qui lance `self-update`, puis `check`, sans rien installer ; et une unité `drakbase-stack-request@<app>.path` par stack dont la version qui tourne a `app_control`, qui lance `request <app>` dès qu'une demande arrive |
| `self-update` | Met à jour l'agent lui-même (app `drakbase-stack`, artefact `bin`) |

**Codes de sortie** : `0` à jour ou mis à jour, `1` échec, `2` usage, `3` permission à accepter. Pour plusieurs stacks, un échec l'emporte sur une permission.

## Fichiers

```
/etc/drakbase-stack/config.json                    serveur, identifiant d'installation (UUID v4), statistiques
/var/lib/drakbase-stack/<app>/                     dossier de la stack (0700, à root)
/var/lib/drakbase-stack/<app>/state.json           serveur et canal, versions installée, précédente, en échec et écartées,
                                                   clés de confiance, permissions acceptées, images de chaque version
/var/lib/drakbase-stack/<app>/releases/<version>/  contenu de l'archive (droits 0644 et 0755)
/var/lib/drakbase-stack/<app>/.env                 réglages de l'utilisateur (0600)
/var/lib/drakbase-stack/<app>/.lock                verrou de la stack (flock)
/var/lib/drakbase-stack/<app>/control/             dossier de contrôle, pour une stack qui a app_control
```

Le dossier de chaque stack est en 0700 : aucun autre compte de la machine n'y entre. L'agent le crée ainsi, et l'y remet à chaque prise du verrou de la stack. Sans root, une commande s'arrête donc sur « accès refusé ».

## Variables d'environnement

Pour les essais seulement :

| Variable | Remplace |
|---|---|
| `DRAKBASE_STACK_CONFIG_DIR` | `/etc/drakbase-stack` |
| `DRAKBASE_STACK_STATE_DIR` | `/var/lib/drakbase-stack` |
| `DRAKBASE_STACK_SYSTEMD_DIR` | `/etc/systemd/system` |
| `DRAKBASE_STACK_DOCKER` | la commande `docker` |
| `DRAKBASE_STACK_SYSTEMCTL` | la commande `systemctl` |

Docker Compose ne reçoit que des variables choisies (`PATH`, `HOME`, la langue, et de quoi joindre Docker : `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG`…). Un compose peut les lire et les donner à son conteneur : aucune ne porte de secret, et ni les proxys ni `DOCKER_AUTH_CONFIG` n'en sont (les identifiants d'un registre vont dans `docker login`). Elles sont choisies parce qu'une variable du shell l'emporterait sur le `.env` dans l'interpolation, et `COMPOSE_FILE` ou `COMPOSE_PROFILES` changeraient le projet. Le compose contrôlé est ainsi celui qui démarre, que l'agent soit lancé à la main ou par le minuteur.

## Choix faits en plus du contrat

- **`.env` et permissions** : le `.env` de la nouvelle version (variables nouvelles ajoutées) est préparé à côté (`.env.next`) avant le contrôle du compose. Les permissions sont donc celles qui démarreront, et le vrai `.env` ne change qu'à l'étape 6.
- **`.env` d'une version qui n'aboutit pas** : seul le succès garde le `.env` complété. Si la version s'arrête après l'étape 6 (`pull`, préparation, démarrage), le `.env` d'avant revient, octet pour octet ; avec `rollback: safe`, avant la relance de la version précédente. Changé à la main entre-temps, il garde ces changements, et seuls les réglages ajoutés par la version en sont retirés. Une première installation qui échoue au démarrage garde le sien : il n'y a pas de version à relancer. Un agent arrêté de force pendant ce temps ne remet rien.
- **Relance d'une version gardée** (retour seul, `rollback`, `restart`) : elle est toujours contrôlée d'abord, comme une version qui arrive, avec le `.env` actuel. Une permission nouvelle arrête (code 3) sans rien relancer. Le retour seul ne peut pas l'accepter : la version qui vient d'échouer reste alors en échec, et `rollback --accept` fait le retour.
- **Compose** : le `compose.yaml` de l'archive est contrôlé deux fois. Tel qu'il est écrit d'abord, avant que Docker Compose ne le lise : c'est le JSON que publie l'outil, sans `include` ni `extends`, et ses `env_file` sont dans `files/`. Dans sa forme résolue ensuite, où tout champ hors de la liste du contrat est refusé (`compose_field_unsupported`), même inconnu de cette version de l'agent.
- **Étape 8** : une première installation ne lance pas `pre_update`, car il n'y a rien à sauvegarder ni à migrer.
- **`pull`** se fait avec `--profile '*'`, pour avoir aussi l'image de `pre_update` ; **`run`** avec `--profile drakbase-hooks run --rm -T`.
- **Échecs aux étapes 2 à 8** (téléchargement, signature, archive, compose, `pull`, préparation) : la version n'est pas écartée, et la vérification suivante réessaie. Seul l'échec à l'étape 9 l'écarte.
- **Version en échec** (`rollback: manual`) : elle est notée `failed` dans `state.json`. `update` le rappelle (code 1). Une version plus récente peut s'installer par-dessus, pas une plus ancienne, qui demande d'abord un `rollback`.
- **Version examinée à côté** : une version est extraite et contrôlée dans `releases/.<version>.next`, puis renommée en `releases/<version>` une fois ses permissions acceptées. La reprise (`update --retry`) d'une version restée en échec ne lui retire donc jamais son dossier : si elle s'arrête avant le démarrage, `rollback` reste possible. Un agent arrêté au milieu du renommage laisse l'ancien dossier de côté (`releases/.<version>.old`) : la commande suivante le remet, sous le verrou.
- **Versions écartées** : une version écartée le reste tant qu'un `rollback` peut ramener sous elle. Seules celles plus anciennes que la précédente gardée sont oubliées.
- **Images** : `state.json` retient les images du compose résolu de chaque version gardée, pour supprimer celles qu'aucune version gardée n'utilise plus.
- **`rollback`** garde les clés de confiance de la dernière version vérifiée : elle les tenait d'une version signée par une clé de confiance.
- **`--no-stats`** coupe les statistiques pour toute la machine (`stats: false` dans `config.json`).
- **`config.json`** est créé par la première commande qui en a besoin, avec le serveur de `--server`, sinon `https://updates.drakbase.com`. Chaque stack garde son propre serveur dans `state.json`.
- **`self-update`** refuse un exécutable annoncé à plus de 256 Mio. Une archive de stack annoncée au-delà de 17 Mio (`MAX_ARCHIVE_BYTES`) est refusée.
- **Tailles** : elles s'écrivent comme dans l'outil `drakbase` (`am_protocol::human_size`), en unités binaires : `25,2 Mio`.

## Clés de `self-update`

Elles sont lues à la compilation dans `crates/am-agent/drakbase.json` (`public_keys`, au format Tauri), que `drakbase init` écrit. Sans ce fichier, l'exécutable n'embarque aucune clé, et `self-update` refuse en l'expliquant. Il faut alors installer la nouvelle version à la main. Les clés embarquées sont contrôlées par le test `embedded_keys_are_valid`, et le refus sans clé par `refuses_without_an_embedded_key`.

## Construction pour Linux

```bash
scripts/build-agent.sh [dossier]    # dist/ par défaut
```

Le script passe par Docker (`docker/Dockerfile`, cible `agent`, cargo-zigbuild, sans émulation) et produit `drakbase-stack-<version>-aarch64` et `drakbase-stack-<version>-x86_64`. Ce sont les noms des artefacts `bin`. Chaque exécutable est ensuite lancé seul dans une image vide de son arch (`--version`, `--help`) : il ne démarre que s'il est vraiment statique.

## Essais

- `cargo test -p am-agent` : tests unitaires, et parcours du binaire (`tests/agent.rs`) contre un faux serveur (`tests/support/fake_server.rs`, axum) et un faux `docker` en script. Ce faux docker journalise ses appels et rend des sorties préparées. Rien n'est écrit hors des dossiers temporaires, et le vrai Docker n'est pas lancé.
- `crates/am-agent/trial/linux.sh [docker:dind|docker:24-dind]` : essai réel, à part. Il monte un Docker neuf en arm64 et y lance l'exécutable statique. Le faux serveur de l'essai (`examples/trial_server.rs`) sert la stack « demo », signée par la clé d'essai, et son image en registre. Le parcours couvre l'installation, le dossier de la stack fermé aux autres comptes, le dossier de contrôle vu de l'app, `check`, les demandes de l'interface (`request` lancé à la main, faute de systemd), `restart`, l'échec avec retour seul et le `.env` remis comme avant, la permission nouvelle, la version restée en échec puis reprise (`update --retry`), le `rollback` arrêté par un réglage changé à la main puis accepté (`--accept`), et la désinstallation. Le script reconstruit l'exécutable s'il est plus ancien que ses sources, et retire tout à la fin.

## Mises à jour validées dans l'app

Rien ne s'installe sans qu'une personne le demande : `sudo drakbase-stack update` sur la machine, ou la validation dans l'interface de l'app. Une stack dont `stack.json` a `app_control`, et elle seule, reçoit, monté dans son service, le dossier `/var/lib/drakbase-stack/<app>/control/` :
- `status.json`, en lecture seule : version installée, version disponible (déjà vérifiée), permissions nouvelles, dernière demande ;
- `request/`, en écriture : l'app y dépose `{"format": 1, "action": "check"}` ou `{"format": 1, "action": "update", "version": "…"}`.

L'agent ajoute ces deux montages (le dossier en lecture seule, `request/` par-dessus en écriture) par son propre fichier Compose (`drakbase-control.json`, dans le dossier de la version), que le contrôle des permissions ne voit pas. Une version qui demande une permission nouvelle ne s'installe jamais depuis l'interface, et une demande n'installe que la version que la dernière vérification a proposée : si le serveur en propose une autre entre-temps, rien n'est installé.

Le dossier de contrôle et l'unité des demandes suivent la version qui tourne (celle restée en échec, sinon l'installée) : après chaque démarrage réussi, une version sans `app_control` perd l'un et l'autre, une version qui l'a les reçoit. `request` refuse une stack dont la version qui tourne n'a pas `app_control`, sans lire ni créer `control/`. `request/` est en 0777 pour l'utilisateur du conteneur ; le dossier de la stack, en 0700, n'y laisse entrer aucun autre compte de la machine. Un dossier de stack trouvé ouvert (celui d'un agent plus ancien) est refermé, et `request/` vidé sans être lu.

`request` ne laisse jamais de demande derrière une erreur, que l'unité relancerait sans fin : il les retire et note l'échec pour l'app, ou désactive l'unité quand l'état de la stack ne se lit plus.

L'agent tourne en root et ne se fie pas à ce que contient ce dossier : il n'y suit aucun lien, ne lit que des fichiers ordinaires de 4 Kio au plus, et prépare `status.json` hors de `control/`. Une demande en cours d'écriture (nom en `.…`) est retirée au bout d'une minute, ou tout de suite si sa date est dans le futur. Une stack installée par un agent plus ancien garde `control/` monté en écriture jusqu'à sa prochaine mise à jour ou relance ; sans `app_control`, elle garde son `control/` jusqu'à sa prochaine mise à jour, sa prochaine relance ou le prochain `auto on`. Le détail est dans docs/PROTOCOLE.md, « Mises à jour validées dans l'app ».
