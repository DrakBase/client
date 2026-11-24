"""Client minimal des API de DrakBase - Apps Market pour les essais (docs/API.md) : connexion avec
enrôlement TOTP, mode sudo, création de l'app, de sa clé et d'un jeton, puis retrait d'une
version par le jeton. Bibliothèque standard seulement (Python 3.9).
"""

import base64
import hashlib
import hmac
import json
import struct
import time
import urllib.error
import urllib.request

SESSION_COOKIE = "__Host-am_session"


class ApiError(Exception):
    def __init__(self, status, body):
        super().__init__(f"HTTP {status} : {body}")
        self.status = status
        self.body = body


def totp(secret_base32, step):
    """Code TOTP du pas donné (RFC 6238 : SHA-1, 6 chiffres, pas de 30 s, comme le serveur)."""
    key = base64.b32decode(secret_base32.upper() + "=" * (-len(secret_base32) % 8))
    digest = hmac.new(key, struct.pack(">Q", step), hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    value = struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF
    return f"{value % 1_000_000:06d}"


def _send(request):
    """Envoie la requête ; renvoie (statut, en-têtes, corps lu en JSON s'il y en a un)."""
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            status, headers, raw = response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            body = json.loads(raw)
        except ValueError:
            body = raw.decode("utf-8", "replace")
        raise ApiError(error.code, body) from None
    return status, headers, (json.loads(raw) if raw else None)


class AdminSession:
    """Session d'administration : cookie, jeton CSRF et `Origin` posés à la main, car le cookie
    `__Host-` est `Secure` et l'essai parle en HTTP à 127.0.0.1."""

    def __init__(self, base_url, origin):
        self.base_url = base_url.rstrip("/")
        self.origin = origin.rstrip("/")
        self.cookie = None
        self.csrf = None

    def call(self, method, path, body=None):
        headers = {"Origin": self.origin, "Accept": "application/json"}
        data = None
        if method not in ("GET", "HEAD"):
            data = json.dumps({} if body is None else body).encode()
            headers["Content-Type"] = "application/json"
            if self.csrf:
                headers["X-CSRF-Token"] = self.csrf
        if self.cookie:
            headers["Cookie"] = f"{SESSION_COOKIE}={self.cookie}"
        request = urllib.request.Request(
            self.base_url + path, data=data, method=method, headers=headers
        )
        _, response_headers, payload = _send(request)
        # Le serveur change le cookie à la connexion et après la 2FA.
        for cookie in response_headers.get_all("Set-Cookie") or []:
            name, _, rest = cookie.partition("=")
            if name.strip() == SESSION_COOKIE:
                self.cookie = rest.split(";", 1)[0] or None
        if isinstance(payload, dict) and payload.get("csrf_token"):
            self.csrf = payload["csrf_token"]
        return payload


def bootstrap(base_url, origin, email, password, app):
    """Premier passage d'un superadmin tout juste créé : enrôlement TOTP, mode sudo, app, clé
    active et jeton de publication limité à l'app. Renvoie le secret du jeton (`amk_…`)."""
    session = AdminSession(base_url, origin)
    login = session.call("POST", "/api/admin/auth/login", {"email": email, "password": password})
    if login.get("stage") != "mfa_enroll":
        raise RuntimeError(f"étape inattendue après la connexion : {login.get('stage')}")
    secret = session.call("POST", "/api/admin/me/totp/setup")["secret"]
    step = int(time.time()) // 30
    session.call("POST", "/api/admin/me/totp/confirm", {"code": totp(secret, step)})
    # Le serveur refuse un pas déjà utilisé, et tolère un pas d'avance.
    session.call("POST", "/api/admin/auth/reauth", {"password": password, "code": totp(secret, step + 1)})
    session.call(
        "POST",
        "/api/admin/apps",
        {
            "slug": app["slug"],
            "name": app["name"],
            "kind": "tauri",
            "bundle_id": app["bundle_id"],
            "description": app["description"],
        },
    )
    session.call("PATCH", f"/api/admin/apps/{app['slug']}", {"listed": True})
    key = session.call(
        "POST",
        f"/api/admin/apps/{app['slug']}/keys",
        {"public_key": app["public_key"], "status": "active"},
    )
    token = session.call(
        "POST",
        "/api/admin/me/tokens",
        {
            "name": "essai de bout en bout",
            "scopes": ["releases:write", "releases:publish"],
            "app": app["slug"],
            "expires_in_days": 1,
        },
    )
    return {"token": token["secret"], "key_id": key["key_id"]}


class PublishApi:
    """API de publication, avec le jeton (`Authorization: Bearer amk_…`)."""

    def __init__(self, base_url, token):
        self.base_url = base_url.rstrip("/")
        self.token = token

    def call(self, method, path, body=None):
        headers = {"Authorization": f"Bearer {self.token}", "Accept": "application/json"}
        data = None
        if method not in ("GET", "HEAD"):
            data = json.dumps({} if body is None else body).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            self.base_url + path, data=data, method=method, headers=headers
        )
        return _send(request)[2]

    def yank(self, app, version, reason):
        return self.call(
            "POST", f"/api/v1/publish/apps/{app}/releases/{version}/yank", {"reason": reason}
        )

    def app(self, app):
        return self.call("GET", f"/api/v1/publish/apps/{app}")


def status_of(url, method="GET"):
    """Statut HTTP d'une adresse, sans lever d'erreur pour un 4xx."""
    request = urllib.request.Request(url, method=method)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            response.read()
            return response.status
    except urllib.error.HTTPError as error:
        return error.code
