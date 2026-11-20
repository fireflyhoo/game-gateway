#!/usr/bin/env python3
"""
token_server.py —— 为泡泡大乱斗签发 JWT（game=bubble）

- 读取 /opt/game-gateway/.env 中的 JWT_SECRET，与网关共享密钥
- GET /token?nick=xxx&uid=xxx  ->  { token, nick, game, exp, identity }
- 纯标准库实现 HS256 JWT，无第三方依赖
- 监听 127.0.0.1:8081，由 Caddy 反代到 api.coollf.com/token
"""
import os
import re
import json
import time
import hmac
import hashlib
import base64
import secrets
import socketserver
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs


class ThreadingHTTPServer(socketserver.ThreadingMixIn, HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

ENV_PATH = "/opt/game-gateway/.env"
GAME = "bubble"
TTL_SECS = 7200


def load_secret():
    secret = os.environ.get("JWT_SECRET")
    if secret:
        return secret
    with open(ENV_PATH, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line.startswith("JWT_SECRET="):
                return line.split("=", 1)[1]
    raise RuntimeError("JWT_SECRET not found")


SECRET = load_secret().encode("utf-8")


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def sign_jwt(claims: dict) -> str:
    header = {"alg": "HS256", "typ": "JWT"}
    h = b64url(json.dumps(header, separators=(",", ":")).encode())
    p = b64url(json.dumps(claims, separators=(",", ":")).encode())
    signing_input = (h + "." + p).encode("ascii")
    sig = b64url(hmac.new(SECRET, signing_input, hashlib.sha256).digest())
    return h + "." + p + "." + sig


class Handler(BaseHTTPRequestHandler):
    server_version = "BubbleToken/1.0"

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")

    def _json(self, obj, code=200):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self._cors()
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.end_headers()

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path != "/token":
            self._json({"error": "not found"}, 404)
            return
        qs = parse_qs(parsed.query)
        nick = (qs.get("nick", [""])[0] or "").strip()
        uid = (qs.get("uid", [""])[0] or "").strip()
        if not nick:
            self._json({"error": "missing nick"}, 400)
            return
        if len(nick) > 12:
            nick = nick[:12]
        # 身份：优先用客户端持久 uid，否则随机
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", uid):
            uid = "u_" + secrets.token_hex(8)
        identity = "bb_" + uid
        now = int(time.time())
        claims = {
            "sub": identity,
            "nick": nick,
            "game": GAME,
            "iat": now,
            "exp": now + TTL_SECS,
        }
        token = sign_jwt(claims)
        self._json({
            "token": token,
            "identity": identity,
            "nick": nick,
            "game": GAME,
            "exp": now + TTL_SECS,
        })

    def log_message(self, fmt, *args):
        pass  # 静默，交给 journald


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", 8081), Handler)
    server.serve_forever()
