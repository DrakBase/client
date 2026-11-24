"""Relais HTTP de l'essai, placé entre l'app et le serveur.

Il transmet chaque requête, note dans un journal (une ligne JSON par requête) la méthode, le
chemin, le statut et les en-têtes qui intéressent l'essai, et peut, à la demande :
- altérer la réponse de vérification, comme le ferait un serveur compromis ;
- retenir les téléchargements, pour retirer une version pendant qu'un client la télécharge.

Bibliothèque standard seulement (Python 3.9).
"""

import http.client
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOGGED_HEADERS = (
    "x-appsmarket-channel",
    "x-appsmarket-install-id",
    "x-appsmarket-os-version",
    "range",
    "accept",
    "user-agent",
)
# En-têtes propres à une connexion, que le relais ne transmet pas.
HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
}


class Relay:
    def __init__(self, listen_port, upstream_port, log_path):
        self.listen_port = listen_port
        self.upstream_port = upstream_port
        self.log_path = log_path
        self.lock = threading.Lock()
        # Fonction (dict -> dict) appliquée aux réponses 200 de /api/v1/update, ou None.
        self.tamper = None
        # Event tant que les téléchargements sont retenus, None sinon.
        self.hold = None
        self.server = None

    def start(self):
        relay = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                relay.forward(self)

            def do_HEAD(self):
                relay.forward(self)

        self.server = ThreadingHTTPServer(("127.0.0.1", self.listen_port), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self):
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()

    def log(self, entry):
        entry = {"t": round(time.time() * 1000), **entry}
        with self.lock:
            with open(self.log_path, "a", encoding="utf-8") as file:
                file.write(json.dumps(entry, ensure_ascii=False) + "\n")

    def entries(self):
        with self.lock:
            try:
                with open(self.log_path, encoding="utf-8") as file:
                    return [json.loads(line) for line in file if line.strip()]
            except FileNotFoundError:
                return []

    def forward(self, handler):
        path = handler.path
        headers = {
            name.lower(): value
            for name, value in handler.headers.items()
            if name.lower() in LOGGED_HEADERS
        }
        entry = {"method": handler.command, "path": path, "headers": headers}

        hold = self.hold
        if path.startswith("/dl/") and hold is not None:
            self.log({**entry, "held": True})
            hold.wait(timeout=120)

        upstream = http.client.HTTPConnection("127.0.0.1", self.upstream_port, timeout=120)
        forwarded = {
            name: value
            for name, value in handler.headers.items()
            if name.lower() not in HOP_BY_HOP
        }
        upstream.request(handler.command, path, headers=forwarded)
        response = upstream.getresponse()
        body = None
        tampered = False
        if (
            self.tamper is not None
            and path.startswith("/api/v1/update/")
            and response.status == 200
        ):
            original = json.loads(response.read())
            body = json.dumps(self.tamper(original)).encode()
            tampered = True
        try:
            handler.send_response(response.status, response.reason)
            for name, value in response.getheaders():
                lower = name.lower()
                if lower in HOP_BY_HOP or (body is not None and lower == "content-length"):
                    continue
                handler.send_header(name, value)
            if body is not None:
                handler.send_header("Content-Length", str(len(body)))
            # Une connexion par requête : la fin du corps est la fin de la connexion.
            handler.send_header("Connection", "close")
            handler.close_connection = True
            handler.end_headers()
            sent = 0
            if handler.command != "HEAD":
                if body is not None:
                    handler.wfile.write(body)
                    sent = len(body)
                else:
                    while True:
                        chunk = response.read(256 * 1024)
                        if not chunk:
                            break
                        handler.wfile.write(chunk)
                        sent += len(chunk)
        finally:
            upstream.close()
        self.log({**entry, "status": response.status, "bytes": sent, "tampered": tampered})
