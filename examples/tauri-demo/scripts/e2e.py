#!/usr/bin/env python3
"""Essai de bout en bout de la démo Tauri (README.md, « Essai de bout en bout sur macOS » et
« Essai sous Linux »).

Un vrai serveur DrakBase - Apps Market tourne sur le Mac ; l'app y est créée par l'API d'administration
(superadmin, TOTP, mode sudo, clé, jeton). Les versions sont construites par `tauri build`,
signées par Tauri avec la clé d'essai, publiées par `drakbase publish`, puis installées par le
plugin updater de l'app elle-même.

- macOS (par défaut) : l'app est installée dans un dossier temporaire et lancée par `open -n`.
- Linux (`--linux`) : l'AppImage est construite et lancée dans un conteneur Docker (OrbStack),
  avec Xvfb et une session D-Bus ; elle joint le serveur du Mac par `host.docker.internal`.

Un relais entre l'app et le serveur note les requêtes, et sait altérer une réponse ou retenir un
téléchargement. L'app écrit ses étapes dans un journal (`--journal`).

Utilisation : python3 scripts/e2e.py [--linux] [--work <dossier>] [--reuse-builds]
"""

import argparse
import base64
import hashlib
import json
import os
import plistlib
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import admin
from relay import Relay

SCRIPTS = Path(__file__).resolve().parent
DEMO = SCRIPTS.parent
REPO = DEMO.parents[1]
SERVER_BIN = REPO / "target" / "release" / "appsmarket-server"
CLI_BIN = REPO / "target" / "release" / "drakbase"

PRODUCT = "Tauri Demo"
APP = {
    "slug": "tauri-demo",
    "name": "Démo Tauri",
    "bundle_id": "fr.example.tauridemo",
    "description": "App Tauri d'exemple, mise à jour par DrakBase - Apps Market",
    "public_key": (REPO / "fixtures" / "keys" / "test-app.pub").read_text().strip(),
}
# Clé d'essai de fixtures/keys : son mot de passe est public (fixtures/README.md).
SIGNING_KEY = REPO / "fixtures" / "keys" / "test-app.secret"
SIGNING_PASSWORD = "appsmarket-fixtures"
ADMIN_EMAIL = "essai@example.fr"
ADMIN_PASSWORD = "essai-de-bout-en-bout"
UUID_V4 = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
# Une version de système que le serveur sait lire : elle commence par des nombres séparés par des
# points (docs/PROTOCOLE.md, « Versions d'OS »).
OS_VERSION = re.compile(r"^[0-9]+(\.[0-9]+)*")
# L'adresse du relais est inscrite dans l'app à la construction : elle ne change pas d'un essai
# à l'autre, pour que --reuse-builds reste possible.
SERVER_PORT = 47810
RELAY_PORT = 47811
LINUX_IMAGE = "appsmarket-tauri-demo-linux:1"
LINUX_CONTAINER = "appsmarket-tauri-demo-e2e"
LINUX_ENV = ["-e", "DISPLAY=:99", "-e", "DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/dbus.sock"]


def key_id(public_key):
    """Identifiant d'une clé publique au format Tauri, en hexadécimal comme l'écrit l'outil."""
    data = base64.b64decode(base64.b64decode(public_key).decode().splitlines()[1])
    return data[2:10][::-1].hex().upper()


def tool_env(**extra):
    env = os.environ.copy()
    env["PATH"] = os.pathsep.join(
        ["/opt/homebrew/opt/rustup/bin", "/opt/homebrew/bin", env.get("PATH", "")]
    )
    env.update({key: str(value) for key, value in extra.items()})
    return env


def run(command, env=None, cwd=None, stdin=None, capture=False):
    command = [str(part) for part in command]
    print("  $ " + " ".join(shlex.quote(part) for part in command), flush=True)
    result = subprocess.run(
        command, env=env, cwd=cwd, input=stdin, text=True, capture_output=capture
    )
    if result.returncode != 0:
        output = (result.stdout or "") + (result.stderr or "") if capture else ""
        raise SystemExit(f"échec de {command[0]} (code {result.returncode})\n{output}")
    return result


def output_of(command):
    """Sortie d'une commande d'inspection, qui n'est pas affichée."""
    return subprocess.run(
        [str(part) for part in command], capture_output=True, text=True, check=True
    ).stdout


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def now_ms():
    return round(time.time() * 1000)


class Checks:
    """Contrôles de l'essai : chacun est affiché, et le premier qui échoue arrête tout."""

    def __init__(self):
        self.passed = []

    def section(self, title):
        print(f"\n== {title}", flush=True)

    def ok(self, condition, label, detail=None):
        if not condition:
            message = f"ÉCHEC : {label}"
            if detail is not None:
                message += "\n" + (
                    detail
                    if isinstance(detail, str)
                    else json.dumps(detail, indent=2, ensure_ascii=False)
                )
            raise SystemExit(message)
        self.passed.append(label)
        print(f"  ✔ {label}", flush=True)


def wait_until(predicate, timeout, what, describe=None):
    deadline = time.time() + timeout
    while True:
        value = predicate()
        if value:
            return value
        if time.time() > deadline:
            detail = describe() if describe else ""
            raise SystemExit(f"ÉCHEC : délai dépassé ({timeout} s) en attendant {what}\n{detail}")
        time.sleep(0.25)


class Journal:
    """Journal écrit par l'app (une ligne JSON par événement, voir src-tauri/src/e2e.rs)."""

    def __init__(self, path):
        self.path = path

    def entries(self):
        try:
            with open(self.path, encoding="utf-8") as file:
                lines = [line for line in file.read().split("\n") if line.strip()]
        except FileNotFoundError:
            return []
        entries = []
        for index, line in enumerate(lines):
            try:
                entries.append(json.loads(line))
            except ValueError:
                # Une dernière ligne en cours d'écriture sera lue au tour suivant.
                if index != len(lines) - 1:
                    raise
        return entries

    def dump(self):
        return "\n".join(json.dumps(entry, ensure_ascii=False) for entry in self.entries())

    def wait(self, predicate, timeout, what):
        """Attend que `predicate(entrées)` soit vrai, et renvoie les entrées de ce moment."""

        def ready():
            entries = self.entries()
            return entries if predicate(entries) else None

        return wait_until(ready, timeout, what, self.dump)


def starts(entries):
    return [entry for entry in entries if entry["event"] == "start"]


def phases(entries, pid):
    return [entry for entry in entries if entry["event"] == "phase" and entry["pid"] == pid]


def phase_names(entries, pid):
    return [entry["phase"] for entry in phases(entries, pid)]


def first_error(entries):
    return next(
        (entry for entry in entries if entry["event"] == "phase" and entry["phase"] == "error"),
        None,
    )


# Plateformes ---------------------------------------------------------------------------------


class MacOS:
    name = "macOS"
    target = "darwin"
    arch = "aarch64" if os.uname().machine == "arm64" else "x86_64"
    bundle_query = "app"
    bundle_type = "App"
    settings_file = (
        Path.home() / "Library" / "Application Support" / APP["bundle_id"] / "appsmarket.json"
    )
    bundle_dir = DEMO / "src-tauri" / "target" / "release" / "bundle" / "macos"

    def __init__(self, work):
        self.work = work
        self.builds = work / "builds"
        self.public_url = f"http://127.0.0.1:{RELAY_PORT}"
        self.prepared = False

    def prepare(self):
        # `npm ci` recopie l'aide de clients/tauri : `npm install` garderait l'ancienne copie.
        run(["npm", "ci", "--no-audit", "--no-fund"], env=tool_env(), cwd=DEMO)

    def artifact(self, version):
        return self.builds / version / f"{PRODUCT}.app.tar.gz"

    def build(self, version, endpoint):
        out = self.builds / version
        if out.exists():
            shutil.rmtree(out)
        config = self.builds / f"config-{version}.json"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(json.dumps(build_config(version, endpoint), indent=2))
        if not self.prepared:
            self.prepare()
            self.prepared = True
        env = tool_env(
            TAURI_SIGNING_PRIVATE_KEY=SIGNING_KEY.read_text().strip(),
            TAURI_SIGNING_PRIVATE_KEY_PASSWORD=SIGNING_PASSWORD,
        )
        run(
            ["npx", "tauri", "build", "--ci", "--bundles", "app", "--config", config],
            env=env,
            cwd=DEMO,
        )
        out.mkdir(parents=True)
        run(["ditto", self.bundle_dir / f"{PRODUCT}.app", out / f"{PRODUCT}.app"])
        for name in (f"{PRODUCT}.app.tar.gz", f"{PRODUCT}.app.tar.gz.sig"):
            shutil.copy2(self.bundle_dir / name, out / name)

    def installation(self, name):
        return MacInstallation(self, self.work / f"installation-{name}")

    def read_settings(self):
        return json.loads(self.settings_file.read_text())

    def start(self):
        # Première installation : pas encore de réglages ni d'identifiant.
        if self.settings_file.exists():
            self.settings_file.unlink()

    def stop(self):
        for root in self.work.glob("installation-*"):
            for pid in MacInstallation(self, root).processes():
                try:
                    os.kill(pid, 15)
                except ProcessLookupError:
                    pass


class MacInstallation:
    """Une copie de l'app dans un dossier temporaire, comme un utilisateur l'aurait installée."""

    def __init__(self, platform, root):
        self.platform = platform
        self.root = root
        self.app = root / f"{PRODUCT}.app"

    def install(self, version):
        if self.root.exists():
            shutil.rmtree(self.root)
        self.root.mkdir(parents=True)
        run(["ditto", self.platform.builds / version / f"{PRODUCT}.app", self.app])

    def version(self):
        with open(self.app / "Contents" / "Info.plist", "rb") as file:
            return plistlib.load(file)["CFBundleShortVersionString"]

    def launch(self, journal, *args):
        # Comme un double-clic : LaunchServices ouvre une nouvelle instance.
        run(["open", "-n", self.app, "--args", "--journal", journal.path, *args])

    def processes(self):
        prefix = str(self.app / "Contents" / "MacOS") + "/"
        pids = []
        for line in output_of(["ps", "-axo", "pid=,command="]).splitlines():
            pid, _, command = line.strip().partition(" ")
            if command.startswith(prefix):
                pids.append(int(pid))
        return sorted(pids)


class Linux:
    name = "Linux"
    target = "linux"
    bundle_query = "appimage"
    bundle_type = "AppImage"
    settings_file = f"/home/demo/.local/share/{APP['bundle_id']}/appsmarket.json"

    def __init__(self, work):
        self.work = work
        self.builds = work / "builds-linux"
        self.sources = work / "linux-src"
        self.public_url = f"http://host.docker.internal:{RELAY_PORT}"
        self.arch = None
        self.prepared = False

    def prepare(self):
        run(["docker", "build", "-t", LINUX_IMAGE, DEMO / "linux"])
        machine = output_of(["docker", "run", "--rm", LINUX_IMAGE, "uname", "-m"]).strip()
        self.arch = {"aarch64": "aarch64", "arm64": "aarch64", "x86_64": "x86_64"}[machine]
        # Une copie des sources : node_modules et target de Linux ne doivent pas remplacer ceux
        # du Mac.
        excludes = ["node_modules", "dist", "src-tauri/target", "src-tauri/gen"]
        for source, target in (
            (DEMO, self.sources / "examples" / "tauri-demo"),
            (REPO / "clients" / "tauri", self.sources / "clients" / "tauri"),
        ):
            target.mkdir(parents=True, exist_ok=True)
            options = [f"--exclude={pattern}" for pattern in excludes]
            run(["rsync", "-a", "--delete", *options, f"{source}/", f"{target}/"])

    def container_path(self, path):
        return "/e2e/" + str(Path(path).relative_to(self.work))

    def artifact(self, version):
        return self.builds / version / f"{PRODUCT}_{version}_{self.arch}.AppImage"

    def build(self, version, endpoint):
        if not self.prepared:
            self.prepare()
            self.prepared = True
        out = self.builds / version
        if out.exists():
            shutil.rmtree(out)
        config = self.builds / f"config-{version}.json"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(json.dumps(build_config(version, endpoint), indent=2))
        demo = self.sources / "examples" / "tauri-demo"
        env = tool_env(
            TAURI_SIGNING_PRIVATE_KEY=SIGNING_KEY.read_text().strip(),
            TAURI_SIGNING_PRIVATE_KEY_PASSWORD=SIGNING_PASSWORD,
        )
        script = (
            "npm ci --no-audit --no-fund && "
            f"npx tauri build --ci --bundles appimage --config {shlex.quote(self.container_path(config))}"
        )
        run(
            [
                "docker", "run", "--rm",
                "-v", f"{self.work}:/e2e",
                "-e", "CARGO_HOME=/e2e/linux-cargo",
                "-e", "npm_config_cache=/e2e/linux-npm",
                # Les valeurs viennent de l'environnement : elles ne passent pas sur la ligne de
                # commande.
                "-e", "TAURI_SIGNING_PRIVATE_KEY",
                "-e", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
                "-w", self.container_path(demo),
                LINUX_IMAGE, "sh", "-c", script,
            ],
            env=env,
        )
        bundle = demo / "src-tauri" / "target" / "release" / "bundle" / "appimage"
        out.mkdir(parents=True)
        name = f"{PRODUCT}_{version}_{self.arch}.AppImage"
        for file in (name, f"{name}.sig"):
            shutil.copy2(bundle / file, out / file)

    def start(self):
        """Un conteneur qui reste allumé, avec un écran virtuel et une session D-Bus : les
        instances de l'app s'y succèdent pendant les mises à jour."""
        subprocess.run(["docker", "rm", "-f", LINUX_CONTAINER], capture_output=True)
        run([
            "docker", "run", "-d", "--name", LINUX_CONTAINER, "--init",
            "-v", f"{self.work}:/e2e", LINUX_IMAGE, "sleep", "infinity",
        ], capture=True)
        self.exec(
            "sh", "-c",
            "Xvfb :99 -screen 0 1280x800x24 >/tmp/xvfb.log 2>&1 & "
            "dbus-daemon --session --fork --address=unix:path=/tmp/dbus.sock >/dev/null",
        )

    def stop(self):
        subprocess.run(["docker", "rm", "-f", LINUX_CONTAINER], capture_output=True)

    def exec(self, *command, detach=False):
        options = ["-d"] if detach else []
        return run(["docker", "exec", *options, *LINUX_ENV, LINUX_CONTAINER, *command], capture=True)

    def installation(self, name):
        return LinuxInstallation(self, name)

    def read_settings(self):
        return json.loads(output_of(["docker", "exec", LINUX_CONTAINER, "cat", self.settings_file]))


class LinuxInstallation:
    """L'AppImage dans un dossier de l'utilisateur du conteneur, comme un utilisateur l'aurait
    téléchargée ; le plugin la remplace en place."""

    def __init__(self, platform, name):
        self.platform = platform
        self.path = f"/home/demo/Applications/{name}/tauri-demo.AppImage"

    def install(self, version):
        source = self.platform.container_path(self.platform.artifact(version))
        folder = str(Path(self.path).parent)
        self.platform.exec(
            "sh", "-c",
            f"rm -rf {shlex.quote(folder)} && mkdir -p {shlex.quote(folder)} && "
            f"cp {shlex.quote(source)} {shlex.quote(self.path)} && chmod 755 {shlex.quote(self.path)}",
        )

    def version(self):
        installed = output_of(
            ["docker", "exec", LINUX_CONTAINER, "sha256sum", self.path]
        ).split()[0]
        for build in self.platform.builds.iterdir():
            artifact = self.platform.artifact(build.name)
            if artifact.exists() and sha256(artifact) == installed:
                return build.name
        return None

    def launch(self, journal, *args):
        self.platform.exec(
            self.path, "--journal", self.platform.container_path(journal.path), *args, detach=True
        )

    def processes(self):
        # L'AppImage s'extrait puis lance `tauri-demo` ; les processus de WebKit sont ses enfants.
        output = output_of(["docker", "exec", LINUX_CONTAINER, "ps", "-eo", "pid=,args="])
        pids = []
        for line in output.splitlines():
            pid, _, command = line.strip().partition(" ")
            if command == "tauri-demo" or command.startswith("tauri-demo "):
                pids.append(int(pid))
        return sorted(pids)


def build_config(version, endpoint):
    """Configuration ajoutée à tauri.conf.json : la version, et l'adresse du relais."""
    return {
        "version": version,
        "plugins": {
            "updater": {
                "endpoints": [endpoint],
                # Le relais local parle en HTTP ; une vraie app garde https.
                "dangerousInsecureTransportProtocol": True,
            }
        },
    }


# Serveur -------------------------------------------------------------------------------------


class Server:
    def __init__(self, work, public_url):
        self.data = work / "server"
        self.key_file = work / "master.key"
        self.log_path = work / "server.log"
        self.url = f"http://127.0.0.1:{SERVER_PORT}"
        self.public_url = public_url
        self.process = None

    def env(self):
        return tool_env(
            AM_PUBLIC_URL=self.public_url,
            AM_DATA_DIR=self.data,
            AM_LISTEN=f"127.0.0.1:{SERVER_PORT}",
            AM_MASTER_KEY_FILE=self.key_file,
            RUST_LOG="info",
        )

    def start(self):
        self.data.mkdir(parents=True, exist_ok=True)
        self.key_file.write_text(secrets.token_hex(32) + "\n")
        self.key_file.chmod(0o600)
        run(
            [SERVER_BIN, "create-superadmin", "--email", ADMIN_EMAIL, "--password-stdin"],
            env=self.env(),
            stdin=ADMIN_PASSWORD + "\n",
        )
        log = open(self.log_path, "w")
        self.process = subprocess.Popen(
            [str(SERVER_BIN), "serve"], env=self.env(), stdout=log, stderr=subprocess.STDOUT
        )
        wait_until(lambda: admin.status_of(self.url + "/readyz") == 200, 30, "le serveur")

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=40)


# Essai ---------------------------------------------------------------------------------------


class Essai:
    def __init__(self, work, platform, reuse_builds):
        self.work = work
        self.platform = platform
        self.reuse_builds = reuse_builds
        self.checks = Checks()
        self.server = Server(work, platform.public_url)
        self.relay = Relay(RELAY_PORT, SERVER_PORT, work / "relay.log")
        self.publish_api = None
        self.journal_count = 0

    def endpoint(self):
        host = self.platform.public_url
        return (
            f"{host}/api/v1/update/{APP['slug']}"
            "/{{target}}/{{arch}}/{{current_version}}?bundle_type={{bundle_type}}"
        )

    def build(self, version):
        """`tauri build` de la version, avec l'adresse du relais ; la signature est faite par
        Tauri avec la clé d'essai."""
        if self.reuse_builds and self.platform.builds.exists():
            if self.platform.name == "Linux" and self.platform.arch is None:
                self.platform.prepare()
                self.platform.prepared = True
            if self.platform.artifact(version).exists():
                print(f"  (build {version} repris)")
                return
        self.platform.build(version, self.endpoint())

    def cli(self, *args, stdin=None, capture=False):
        env = tool_env(APPSMARKET_CREDENTIALS="file", APPSMARKET_HOME=self.work / "cli-home")
        return run([CLI_BIN, *args], env=env, stdin=stdin, capture=capture)

    def publish(self, version, channel):
        notes = self.work / f"notes-{version}.md"
        notes.write_text(f"- Démo Tauri {version}, canal {channel}\n- Essai de bout en bout\n")
        result = self.cli(
            "publish",
            "--app", APP["slug"],
            "--version", version,
            "--channel", channel,
            "--notes", notes,
            "--tauri-config", DEMO / "src-tauri",
            "--publish",
            self.platform.artifact(version),
            capture=True,
        )
        print(result.stdout + result.stderr, end="", flush=True)
        self.checks.ok(
            f"Clé embarquée par cette version : {key_id(APP['public_key'])} " in result.stdout,
            f"{version} déclare la clé qu'elle embarque, lue dans tauri.conf.json",
            result.stdout,
        )

    def new_journal(self, name):
        self.journal_count += 1
        return Journal(self.work / "journaux" / f"{self.journal_count:02d}-{name}.log")

    def relay_entries(self, since):
        return [entry for entry in self.relay.entries() if entry["t"] >= since]

    def update_checks(self, since):
        return [
            entry
            for entry in self.relay_entries(since)
            if entry["path"].startswith("/api/v1/update/")
        ]

    def check_path(self, version):
        return (
            f"/api/v1/update/{APP['slug']}/{self.platform.target}/{self.platform.arch}"
            f"/{version}?bundle_type={self.platform.bundle_query}"
        )

    # Contrôles réutilisés -------------------------------------------------------------------

    def expect_update(self, install, journal, before, after, *args):
        """Lance l'app en mode automatique et attend qu'elle se soit mise à jour, relancée et
        trouvée à jour. Renvoie le PID de la nouvelle instance."""
        checks = self.checks
        install.launch(journal, "--auto-update", *args)

        def updated(entries):
            news = [entry for entry in starts(entries) if entry["version"] == after]
            return bool(news) and "upToDate" in phase_names(entries, news[-1]["pid"])

        entries = journal.wait(updated, 180, f"la version {after} relancée et à jour")
        first, second = starts(entries)[0], starts(entries)[-1]
        old, new = first["pid"], second["pid"]
        checks.ok(first["version"] == before, f"{before} démarre (PID {old})")
        checks.ok(
            first["bundleType"] == self.platform.bundle_type,
            f"l'app se sait installée en {self.platform.bundle_type}",
            first,
        )
        available = [entry for entry in phases(entries, old) if entry["phase"] == "available"]
        checks.ok(
            bool(available) and available[0]["update"]["version"] == after,
            f"{before} trouve la {after}",
            journal.dump(),
        )
        checks.ok(
            phase_names(entries, old)[-3:] == ["available", "downloading", "installing"],
            "téléchargement puis installation, sans erreur",
            journal.dump(),
        )
        checks.ok(
            any(entry["event"] == "exit" and entry["pid"] == old for entry in entries),
            "l'ancienne instance se ferme proprement (RunEvent::Exit)",
            journal.dump(),
        )
        checks.ok(new != old, f"la {after} démarre dans un nouveau processus (PID {new})")
        checks.ok(install.version() == after, f"l'app installée est la {after}")
        wait_until(
            lambda: install.processes() == [new],
            15,
            "une seule instance",
            lambda: f"processus : {install.processes()}",
        )
        checks.ok(True, "une seule instance tourne, la nouvelle")
        return new

    def wait_up_to_date(self, install, journal, *args):
        install.launch(journal, "--auto-update", *args)
        entries = journal.wait(
            lambda entries: bool(starts(entries))
            and "upToDate" in phase_names(entries, starts(entries)[0]["pid"]),
            60,
            "l'app à jour",
        )
        return starts(entries)[0]["pid"]

    def ping_and_quit(self, install, journal, pid):
        """Seconde instance : elle passe la main à la première puis s'arrête. Puis `--quit`, par
        le même chemin, ferme l'app."""
        checks = self.checks
        before = len(journal.entries())
        install.launch(journal, "--ping")
        entries = journal.wait(
            lambda entries: any(
                entry["event"] == "second-instance"
                and entry["pid"] == pid
                and "--ping" in entry["args"]
                for entry in entries[before:]
            ),
            30,
            "le relais de la seconde instance",
        )
        launched = [entry for entry in entries[before:] if entry["event"] == "launch"]
        checks.ok(
            len(launched) == 1 and launched[0]["pid"] != pid,
            "une seconde instance se lance…",
            journal.dump(),
        )
        second = launched[0]["pid"]
        checks.ok(
            not any(entry["event"] == "start" and entry["pid"] == second for entry in entries),
            "… passe ses arguments à la première, et s'arrête avant de démarrer",
        )
        wait_until(
            lambda: install.processes() == [pid],
            15,
            "la fin de la seconde instance",
            lambda: f"processus : {install.processes()}",
        )
        checks.ok(True, "il ne reste que la première instance")
        self.quit(install, journal, pid)

    def quit(self, install, journal, pid):
        before = len(journal.entries())
        install.launch(journal, "--quit")
        journal.wait(
            lambda entries: any(
                entry["event"] == "exit" and entry["pid"] == pid for entry in entries[before:]
            ),
            30,
            "la fermeture de l'app",
        )
        wait_until(
            lambda: install.processes() == [],
            15,
            "plus aucune instance",
            lambda: f"processus : {install.processes()}",
        )
        self.checks.ok(True, "l'app se ferme, plus aucune instance")

    def expect_error(self, install, journal, version, pattern):
        """Lance l'app en mode automatique et attend que la mise à jour échoue."""
        install.launch(journal, "--auto-update", "--channel", "stable")
        entries = journal.wait(
            lambda entries: first_error(entries) is not None, 120, "l'échec de la mise à jour"
        )
        error = first_error(entries)
        self.checks.ok(
            re.search(pattern, error["error"] or "") is not None,
            f"erreur montrée : « {error['error']} »",
        )
        self.checks.ok(install.version() == version, f"l'app reste en {version}")
        pid = starts(entries)[0]["pid"]
        self.checks.ok(install.processes() == [pid], "l'app tourne toujours, sans relance")
        return pid

    # Déroulé ---------------------------------------------------------------------------------

    def run(self, versions, scenarios):
        checks = self.checks
        checks.section(f"Préparation ({self.platform.name})")
        run(
            ["cargo", "build", "--release", "-p", "am-server", "-p", "am-cli"],
            env=tool_env(),
            cwd=REPO,
        )
        for version in versions:
            self.build(version)
        for version in versions[:2]:
            signature = Path(f"{self.platform.artifact(version)}.sig").read_text()
            trusted = base64.b64decode(signature).decode().splitlines()[2]
            checks.ok(
                trusted.endswith(f"\tversion:{version}"),
                f"tauri build signe la {version} avec « version:{version} »",
                trusted,
            )

        self.relay.start()
        self.server.start()
        self.platform.start()
        token = admin.bootstrap(
            self.server.url, self.server.public_url, ADMIN_EMAIL, ADMIN_PASSWORD, APP
        )
        checks.ok(True, "superadmin, TOTP, mode sudo, app, clé et jeton créés par l'API")
        self.publish_api = admin.PublishApi(self.server.url, token["token"])
        self.cli("login", "--server", self.server.url, "--token-stdin", stdin=token["token"] + "\n")
        self.publish("0.1.0", "stable")
        self.publish("0.1.1", "stable")
        latest = self.publish_api.app(APP["slug"])["latest"]
        checks.ok(
            latest.get("stable") == "0.1.1",
            "0.1.0 et 0.1.1 publiées par drakbase publish",
            latest,
        )
        for scenario in scenarios:
            scenario()
        print(f"\nEssai réussi ({self.platform.name}) : {len(checks.passed)} contrôles. Dossier : {self.work}")

    def scenario_stable(self):
        checks = self.checks
        checks.section("Mise à jour stable : 0.1.0 → 0.1.1, avec le plugin d'instance unique")
        install = self.platform.installation("a")
        install.install("0.1.0")
        journal = self.new_journal("stable")
        since = now_ms()
        pid = self.expect_update(install, journal, "0.1.0", "0.1.1")

        requests = self.relay_entries(since)
        checks_made = [entry for entry in requests if entry["path"].startswith("/api/v1/update/")]
        downloads = [entry for entry in requests if entry["path"].startswith("/dl/")]
        first, last = checks_made[0], checks_made[-1]
        checks.ok(first["path"] == self.check_path("0.1.0"), f"adresse de vérification : {first['path']}")
        checks.ok(
            first["status"] == 200 and last["status"] == 204,
            "le serveur répond 200, puis 204 à la 0.1.1",
        )
        checks.ok(last["path"] == self.check_path("0.1.1"), "la 0.1.1 vérifie à son tour")
        install_id = first["headers"].get("x-appsmarket-install-id", "")
        checks.ok(
            first["headers"].get("x-appsmarket-channel") == "stable" and UUID_V4.match(install_id),
            "en-têtes X-AppsMarket-Channel: stable et X-AppsMarket-Install-Id (UUID v4)",
            first,
        )
        os_version = first["headers"].get("x-appsmarket-os-version", "")
        checks.ok(
            OS_VERSION.match(os_version) is not None,
            f"en-tête X-AppsMarket-OS-Version, la version du système lue par le plugin os : {os_version or 'absent'}",
            first,
        )
        checks.ok(
            last["headers"].get("x-appsmarket-install-id") == install_id,
            "la nouvelle version garde le même identifiant",
        )
        stored = self.platform.read_settings()
        checks.ok(
            stored.get("installId") == install_id,
            f"identifiant rangé dans {self.platform.settings_file}",
            stored,
        )
        checks.ok(
            len(downloads) == 1 and downloads[0]["status"] == 200 and downloads[0]["bytes"] > 0,
            f"un seul téléchargement, complet ({downloads[0]['bytes']} octets)",
            downloads,
        )
        checks.ok(
            "x-appsmarket-install-id" not in downloads[0]["headers"]
            and "x-appsmarket-os-version" not in downloads[0]["headers"],
            "le téléchargement part sans l'identifiant d'installation ni la version du système",
        )
        self.ping_and_quit(install, journal, pid)

    def scenario_beta(self):
        checks = self.checks
        checks.section("Canal bêta : 0.1.2-beta.1 n'est vue que sur le canal bêta")
        self.publish("0.1.2-beta.1", "beta")
        install = self.platform.installation("a")

        journal = self.new_journal("stable-ne-voit-pas-la-beta")
        since = now_ms()
        pid = self.wait_up_to_date(install, journal, "--channel", "stable")
        check = self.update_checks(since)[-1]
        checks.ok(
            check["headers"].get("x-appsmarket-channel") == "stable" and check["status"] == 204,
            "canal stable : le serveur répond 204",
            check,
        )
        self.quit(install, journal, pid)

        journal = self.new_journal("beta")
        since = now_ms()
        pid = self.expect_update(install, journal, "0.1.1", "0.1.2-beta.1", "--channel", "beta")
        available = next(
            entry
            for entry in journal.entries()
            if entry["event"] == "phase" and entry["phase"] == "available"
        )
        checks.ok(
            available["update"]["channel"] == "beta",
            "la mise à jour proposée se dit bêta (rawJson.channel)",
        )
        check = self.update_checks(since)[0]
        checks.ok(
            check["headers"].get("x-appsmarket-channel") == "beta",
            "en-tête X-AppsMarket-Channel: beta",
        )
        self.quit(install, journal, pid)

        journal = self.new_journal("beta-garde")
        since = now_ms()
        pid = self.wait_up_to_date(install, journal)
        check = self.update_checks(since)[-1]
        checks.ok(
            check["headers"].get("x-appsmarket-channel") == "beta",
            "le canal bêta reste choisi d'un lancement à l'autre (réglage rangé)",
            check,
        )
        self.quit(install, journal, pid)

    def scenario_yank(self):
        checks = self.checks
        checks.section("Version retirée : 0.1.3 retirée pendant son téléchargement")
        self.publish("0.1.3", "stable")
        install = self.platform.installation("b")
        install.install("0.1.0")

        journal = self.new_journal("retrait-en-cours")
        since = now_ms()
        self.relay.hold = threading.Event()
        install.launch(journal, "--auto-update", "--channel", "stable")
        held = wait_until(
            lambda: [entry for entry in self.relay_entries(since) if entry.get("held")],
            60,
            "le téléchargement de la 0.1.3",
        )
        self.publish_api.yank(APP["slug"], "0.1.3", "essai : version retirée")
        hold, self.relay.hold = self.relay.hold, None
        hold.set()
        entries = journal.wait(
            lambda entries: first_error(entries) is not None, 60, "l'échec du téléchargement"
        )
        error = first_error(entries)
        download = [
            entry
            for entry in self.relay_entries(since)
            if entry["path"].startswith("/dl/") and "status" in entry
        ][-1]
        checks.ok(download["status"] == 410, "le téléchargement d'une version retirée répond 410")
        checks.ok("retirée" in (error["error"] or ""), f"erreur montrée : « {error['error']} »")
        checks.ok(error["update"] is None, "la version retirée n'est plus proposée")
        checks.ok(install.version() == "0.1.0", "l'app reste en 0.1.0")
        pid = starts(entries)[0]["pid"]
        checks.ok(install.processes() == [pid], "l'app tourne toujours, sans relance")
        self.quit(install, journal, pid)
        checks.ok(
            admin.status_of(f"http://127.0.0.1:{SERVER_PORT}{held[0]['path']}") == 410,
            "l'adresse de la 0.1.3 répond toujours 410",
        )

        journal = self.new_journal("apres-retrait")
        pid = self.expect_update(install, journal, "0.1.0", "0.1.1", "--channel", "stable")
        self.quit(install, journal, pid)

    def scenario_tampered(self):
        self.checks.section("Réponse altérée : requireSignedVersion et signature")
        install = self.platform.installation("c")
        install.install("0.1.0")

        self.relay.tamper = lambda body: {**body, "version": "0.1.9"}
        journal = self.new_journal("version-gonflee")
        pid = self.expect_error(install, journal, "0.1.0", r"la version signée n'est pas celle annoncée")
        self.quit(install, journal, pid)

        other = Path(f"{self.platform.artifact('0.1.0')}.sig").read_text().strip()
        self.relay.tamper = lambda body: {**body, "signature": other}
        journal = self.new_journal("signature-d-une-autre-archive")
        pid = self.expect_error(install, journal, "0.1.0", r"signature invalide")
        self.quit(install, journal, pid)
        self.relay.tamper = None

    def scenario_statistics_off(self):
        checks = self.checks
        checks.section("Statistiques coupées : aucun identifiant envoyé")
        install = self.platform.installation("c")
        install.install("0.1.0")
        journal = self.new_journal("sans-statistiques")
        since = now_ms()
        pid = self.expect_update(
            install, journal, "0.1.0", "0.1.1", "--channel", "stable", "--stats", "off"
        )
        requests = self.update_checks(since)
        checks.ok(
            all("x-appsmarket-install-id" not in entry["headers"] for entry in requests),
            "aucune vérification ne porte X-AppsMarket-Install-Id",
            requests,
        )
        stored = self.platform.read_settings()
        checks.ok(
            stored.get("shareStatistics") is False and "installId" not in stored,
            "réglage rangé, identifiant effacé",
            stored,
        )
        self.quit(install, journal, pid)

    def cleanup(self):
        self.platform.stop()
        self.server.stop()
        self.relay.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--linux", action="store_true", help="essai sous Linux, dans Docker")
    parser.add_argument("--work", type=Path, help="dossier de travail (temporaire par défaut)")
    parser.add_argument(
        "--reuse-builds", action="store_true", help="reprend les builds déjà présents dans le dossier"
    )
    args = parser.parse_args()
    if sys.platform != "darwin":
        raise SystemExit("le serveur et l'outil de l'essai tournent sur macOS")
    work = (args.work or Path(tempfile.mkdtemp(prefix="tauri-demo-e2e-"))).resolve()
    for name in ("server", "journaux", "cli-home", "relay.log", "server.log"):
        target = work / name
        if target.is_dir():
            shutil.rmtree(target)
        elif target.exists():
            target.unlink()
    (work / "journaux").mkdir(parents=True)
    print(f"Dossier de travail : {work}")
    platform = Linux(work) if args.linux else MacOS(work)
    essai = Essai(work, platform, args.reuse_builds)
    if args.linux:
        versions = ["0.1.0", "0.1.1", "0.1.2-beta.1"]
        scenarios = [essai.scenario_stable, essai.scenario_beta, essai.scenario_statistics_off]
    else:
        versions = ["0.1.0", "0.1.1", "0.1.2-beta.1", "0.1.3"]
        scenarios = [
            essai.scenario_stable,
            essai.scenario_beta,
            essai.scenario_yank,
            essai.scenario_tampered,
            essai.scenario_statistics_off,
        ]
    try:
        essai.run(versions, scenarios)
    finally:
        essai.cleanup()


if __name__ == "__main__":
    main()
