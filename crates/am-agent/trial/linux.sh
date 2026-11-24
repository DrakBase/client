#!/usr/bin/env bash
# Essai réel de drakbase-stack sous Linux, à part des tests : un Docker neuf (docker:dind, arm64
# sur un Mac Apple Silicon, comme un Raspberry Pi 5), l'exécutable statique de
# scripts/build-agent.sh, et le faux serveur de l'essai (exemple trial_server) qui sert la stack
# « demo », signée par la clé d'essai, et son image en registre.
#
#   crates/am-agent/trial/linux.sh [image]      (docker:dind par défaut ; docker:24-dind…)
#
# Parcours, dans l'ordre :
#   - installation (code 3 sans --accept, puis --accept) ; dossier de la stack en 0700, fermé aux
#     autres comptes ; dossier de contrôle vu de l'app (status.json lisible, lecture seule) ;
#   - check : la version disponible est montrée, sans être installée ;
#   - demandes de l'interface, sans systemd : l'app dépose une vérification, puis l'installation
#     de la version disponible, et « drakbase-stack request demo » est lancé à la main ;
#   - restart après un réglage changé dans .env ;
#   - version qui ne devient pas saine (retour seul, rollback: safe) : le .env revient comme avant ;
#   - permission nouvelle (code 3, puis --accept) ;
#   - version restée en échec (rollback: manual) : le .env revient comme avant, le réglage qui la
#     rend saine s'ajoute à la main, puis update --retry ;
#   - rollback après un réglage changé dans .env : la version gardée est contrôlée avant d'être
#     relancée (code 3, puis --accept) ;
#   - désinstallation qui garde le volume.
#
# L'exécutable est celui de dist/ s'il est à jour. S'il manque, ou si une de ses sources est plus
# récente que lui, il est reconstruit dans target/trial : dist/ garde ce qui a été publié, avec
# ses signatures. AGENT_BIN désigne un exécutable déjà construit, qui est alors pris tel quel.
#
# Le faux serveur écoute sur 127.0.0.1:8790 (TRIAL_PORT) ; le Docker d'essai le joint par
# host.docker.internal, et le tient pour un registre en HTTP (--insecure-registry). À la fin,
# tout est retiré : conteneur et son volume, faux serveur, dossier de travail. Les images
# docker:dind et alpine:3.24 restent.
set -euo pipefail

cd "$(dirname "$0")/../../.."
if [[ -d /opt/homebrew/opt/rustup/bin ]]; then
  export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi

dind_image=${1:-docker:dind}
port=${TRIAL_PORT:-8790}
host=host.docker.internal:$port
container=drakbase-stack-essai-$$
version=$(scripts/workspace-version)
bin=${AGENT_BIN:-dist/drakbase-stack-$version-aarch64}
# Où reconstruire l'exécutable, sans toucher à dist/.
rebuilt=target/trial
work=$(mktemp -d)
server_pid=''
# Dossier de la stack dans le Docker d'essai.
stack=/var/lib/drakbase-stack/demo

cleanup() {
  if [[ -n $server_pid ]]; then
    kill "$server_pid" 2>/dev/null || true
  fi
  docker rm -f -v "$container" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "✗ $*" >&2
  exit 1
}

# --- Exécutable, image et faux serveur ------------------------------------------------------------
# Ce dont l'exécutable est fait : le workspace, l'agent et le protocole. Ce dossier d'essai n'en
# fait pas partie.
stale_sources() {
  find Cargo.toml Cargo.lock crates/am-agent crates/am-protocol \
    -type f -newer "$1" ! -path 'crates/am-agent/trial/*' -print | head -n 1
}
# Vrai si l'exécutable manque, ou si une de ses sources est plus récente que lui.
outdated() {
  [[ ! -x $1 ]] || [[ -n $(stale_sources "$1" || true) ]]
}
if [[ -z ${AGENT_BIN:-} ]] && outdated "$bin"; then
  bin=$rebuilt/drakbase-stack-$version-aarch64
  if outdated "$bin"; then
    echo "▸ exécutable absent ou plus ancien que ses sources : reconstruction dans $rebuilt"
    scripts/build-agent.sh "$rebuilt"
  fi
fi
[[ -x $bin ]] || fail "exécutable $bin introuvable"
printf 'FROM alpine:3.24\n' >"$work/Dockerfile"
docker buildx build --quiet --platform linux/arm64 --provenance=false --sbom=false \
  --output "type=oci,tar=false,dest=$work/oci" "$work" >/dev/null
cargo build --quiet -p am-agent --example trial_server

# Relance le faux serveur avec ces versions publiées.
serve() {
  if [[ -n $server_pid ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  target/debug/examples/trial_server "127.0.0.1:$port" "http://$host" "$work/oci" "$@" \
    >"$work/server.log" 2>&1 &
  server_pid=$!
  # Prêt quand il l'écrit : toutes ses versions sont alors publiées.
  for _ in $(seq 1 100); do
    grep 'faux serveur prêt' "$work/server.log" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  cat "$work/server.log" >&2
  fail "le faux serveur ne répond pas"
}

# --- Docker neuf --------------------------------------------------------------------------------
docker run -d --privileged --name "$container" "$dind_image" --insecure-registry="$host" >/dev/null
for _ in $(seq 1 60); do
  docker exec "$container" docker info >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$container" docker version --format 'Docker {{.Server.Version}} ({{.Server.Os}}/{{.Server.Arch}})'
echo "Compose $(docker exec "$container" docker compose version --short)"
docker cp "$bin" "$container:/usr/local/bin/drakbase-stack" >/dev/null

# --- Aides du parcours ----------------------------------------------------------------------------
# expect <code attendu> <arguments de drakbase-stack…>
expect() {
  local want=$1
  shift
  echo "▸ drakbase-stack $*"
  set +e
  docker exec "$container" drakbase-stack "$@" 2>&1 | sed 's/^/    /'
  local got=${PIPESTATUS[0]}
  set -e
  [[ $got == "$want" ]] || fail "code $got, attendu $want"
}

# installed <version> : la version en place, saine.
installed() {
  # Sans -q : grep lit tout, et status ne s'arrête pas sur un tube fermé.
  docker exec "$container" drakbase-stack status | grep "^demo $1 " >/dev/null ||
    fail "demo $1 n'est pas la version en place"
  local health
  health=$(docker exec "$container" docker inspect demo-app-1 --format '{{.State.Health.Status}}')
  [[ $health == healthy ]] || fail "demo-app-1 : $health"
}

# status_shows <texte> : la sortie de « drakbase-stack status » contient ce texte.
status_shows() {
  docker exec "$container" drakbase-stack status | grep -F -- "$1" >/dev/null ||
    fail "status ne montre pas « $1 »"
}

# in_app <script sh> [arguments…] : lancé dans le conteneur de l'app, lui-même dans le Docker
# d'essai. L'app y trouve son dossier de contrôle par $DRAKBASE_CONTROL.
in_app() {
  local script=$1
  shift
  docker exec "$container" docker exec demo-app-1 sh -c "$script" sh "$@"
}

# app_reads <texte> : status.json, lu par l'app, contient ce texte. Les espaces et les sauts de
# ligne sont retirés avant la recherche : « "installed":"1.0.0" ».
app_reads() {
  # shellcheck disable=SC2016  # $DRAKBASE_CONTROL est lu dans le conteneur de l'app.
  in_app 'cat "$DRAKBASE_CONTROL/status.json"' | tr -d ' \n' | grep -F -- "$1" >/dev/null ||
    fail "status.json, lu par l'app, ne contient pas $1"
}

# app_asks <nom> <demande JSON> : l'app dépose une demande, sous un nom en « . » puis renommée,
# et l'agent la traite, lancé à la main faute de systemd.
app_asks() {
  echo "▸ l'app demande : $2"
  # shellcheck disable=SC2016  # $1, $2 et $DRAKBASE_CONTROL sont lus dans le conteneur de l'app.
  in_app 'printf "%s\n" "$2" >"$DRAKBASE_CONTROL/request/.$1" &&
    mv "$DRAKBASE_CONTROL/request/.$1" "$DRAKBASE_CONTROL/request/$1.json"' "$1" "$2" ||
    fail "l'app n'a pas pu déposer sa demande"
  expect 0 request demo
  [[ -z $(docker exec "$container" ls -A "$stack/control/request") ]] ||
    fail "des demandes restent dans $stack/control/request"
}

key=$(cat fixtures/keys/test-app.pub)

# --- Installation ---------------------------------------------------------------------------------
serve 1.0.0
expect 3 install demo --key "$key" --server "http://$host"
expect 0 install demo --key "$key" --server "http://$host" --accept
installed 1.0.0

# Le dossier de la stack est fermé aux autres comptes de la machine : ils n'y lisent rien, et ne
# déposent pas de demande.
mode=$(docker exec "$container" stat -c '%a' "$stack")
[[ $mode == 700 ]] || fail "$stack est en $mode, attendu 700"
if docker exec -u 65534:65534 "$container" ls "$stack" >/dev/null 2>&1; then
  fail "un compte sans droit lit $stack"
fi
if docker exec -u 65534:65534 "$container" touch "$stack/control/request/x.json" 2>/dev/null; then
  fail "un compte sans droit dépose une demande dans $stack/control/request"
fi

# Le dossier de contrôle vu de l'app : status.json se lit, et seul request/ s'écrit.
app_reads '"installed":"1.0.0"'
# shellcheck disable=SC2016  # $DRAKBASE_CONTROL est lu dans le conteneur de l'app.
if in_app 'touch "$DRAKBASE_CONTROL/x"' 2>/dev/null; then
  fail "l'app écrit dans son dossier de contrôle, hors de request/"
fi

# --- Vérification, puis demandes de l'interface -----------------------------------------------------
serve 1.0.0 1.1.0
expect 0 check demo
status_shows 'disponible : 1.1.0'
installed 1.0.0
app_reads '"available":{"version":"1.1.0"'

app_asks verification '{"format": 1, "action": "check"}'
app_reads '"action":"check"'
app_reads '"state":"done"'
installed 1.0.0

app_asks installation '{"format": 1, "action": "update", "version": "1.1.0"}'
installed 1.1.0
app_reads '"installed":"1.1.0"'
app_reads '"action":"update"'
app_reads '"state":"done"'
docker exec "$container" grep -x 'TZ=Europe/Paris' "$stack/.env" >/dev/null ||
  fail "TZ absent du .env"

# --- Réglage changé à la main, puis restart --------------------------------------------------------
docker exec "$container" sed -i 's|^TZ=.*|TZ=America/Montreal|' "$stack/.env"
expect 0 restart demo
installed 1.1.0
zone=$(in_app 'printenv TZ')
[[ $zone == America/Montreal ]] || fail "TZ vaut « $zone » dans l'app après restart"

# setting <ligne> : le .env de la stack contient exactement cette ligne.
setting() {
  docker exec "$container" grep -x -- "$1" "$stack/.env" >/dev/null
}

# --- Version qui ne devient pas saine, rollback: safe ----------------------------------------------
# La 1.2.0 ajoute le réglage EXTRA : il ne reste pas dans le .env de la 1.1.0, relancée.
serve 1.0.0 1.1.0 1.2.0
expect 1 update demo
installed 1.1.0
if docker exec "$container" grep '^EXTRA=' "$stack/.env" >/dev/null; then
  fail "le réglage EXTRA de la 1.2.0 est resté dans le .env après le retour à la 1.1.0"
fi
setting 'TZ=America/Montreal' || fail "le .env d'avant la 1.2.0 n'est pas revenu"

# --- Permission nouvelle ---------------------------------------------------------------------------
serve 1.0.0 1.1.0 1.2.0 1.3.0
expect 3 update
expect 0 update demo --accept
installed 1.3.0

# --- Version restée en échec, rollback: manual, puis reprise -----------------------------------------
# La 1.4.0 ne devient saine qu'avec HEALTH=true, et .env.example donne false. Après l'échec, le
# .env est revenu comme avant : le réglage s'y ajoute à la main, et la reprise le garde.
serve 1.0.0 1.1.0 1.2.0 1.3.0 1.4.0
expect 1 update demo
status_shows 'en échec : 1.4.0'
app_reads '"failed":"1.4.0"'
if docker exec "$container" grep '^HEALTH=' "$stack/.env" >/dev/null; then
  fail "le réglage HEALTH de la 1.4.0 est resté dans le .env après son échec"
fi
docker exec "$container" sh -c 'echo HEALTH=true >>"$1"' sh "$stack/.env"
expect 0 update demo --retry
installed 1.4.0
app_reads '"failed":null'
setting 'HEALTH=true' || fail "la reprise n'a pas gardé HEALTH=true"

# --- Retour arrière après un réglage changé à la main ------------------------------------------------
# Avec un autre port, la 1.3.0 demande une permission nouvelle : elle n'est pas relancée sans
# --accept, et la 1.4.0 continue de tourner.
docker exec "$container" sed -i 's|^PORT=.*|PORT=8081|' "$stack/.env"
expect 3 rollback demo
installed 1.4.0
expect 0 rollback demo --accept
installed 1.3.0
docker exec "$container" docker port demo-app-1 8080/tcp | grep ':8081$' >/dev/null ||
  fail "la 1.3.0 relancée ne publie pas le port 8081"
expect 0 status

expect 0 uninstall demo
docker exec "$container" docker volume ls --format '{{.Name}}' | grep -x demo_data >/dev/null ||
  fail "le volume demo_data n'a pas survécu à uninstall"
[[ -z $(docker exec "$container" docker ps -aq) ]] || fail "des conteneurs restent"
echo "✓ parcours réel réussi avec $dind_image"
