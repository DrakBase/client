# Stack de démonstration

Une stack Docker minimale, publiée par DrakBase et installée par l'agent `drakbase-stack` (docs/PLAN.md, « Stacks Docker »). Elle sert d'exemple et d'essai de bout en bout.

- `app/` : l'image, construite par l'outil (`FROM alpine:3.24`, un fichier de version, un contrôle de santé). Au démarrage, elle affiche le réglage `MESSAGE` et le fichier monté.
- `stack/compose.yaml` : un compose ordinaire. Le service `demo` est construit sur place (`build:`) ; l'outil le remplace, à la publication, par l'image épinglée `<serveur>/<app>/demo@sha256:…`. Le service `backup`, au profil `drakbase-hooks`, est lancé par l'agent avant chaque mise à jour.
- `stack/stack.json` : service surveillé, sauvegarde, retour arrière automatique (`safe`) et plateformes. `drakbase init` y ajoute les clés publiques (`public_keys`).
- `stack/.env.example` : les réglages que l'utilisateur peut changer sur sa machine. Ici `MESSAGE`, que le compose passe au conteneur et que l'image affiche.
- `stack/files/` : un fichier monté en lecture seule, affiché lui aussi.

## Publier

```bash
drakbase init demo                 # clés, public_keys, déclaration de l'app sur le serveur
drakbase doctor demo --stack       # règles de la stack, Docker, clés
drakbase release --app demo --stack --version 1.0.0 --channel stable --publish
```

`release` affiche ensuite les commandes d'installation à transmettre.

## Installer, sur une machine Linux avec Docker

```bash
curl -fsSL https://updates.drakbase.com/install/drakbase-stack.sh | sudo sh
sudo drakbase-stack install demo --key <clé publique>
sudo drakbase-stack auto on
```
