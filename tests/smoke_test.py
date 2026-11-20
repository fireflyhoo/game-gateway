#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
game-gateway 冒烟测试：仅用 Python 标准库，不依赖 websocket-client / requests。

验证链路：
  health -> dev-token -> REST 创建房间 -> 两个 WS 客户端加入同一房间
  -> A 发 game 事件 -> B 收到 -> ready -> 房主开局 -> 全员收到 room_state
"""
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import time
import urllib.request
from urllib.parse import urlencode

BASE = os.environ.get("GATEWAY", "http://127.0.0.1:8080").rstrip("/")
GAME = "smoke_" + str(int(time.time()))
PASS = 0
FAIL = 0


def check(cond, label, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  [PASS] {label}")
    else:
        FAIL += 1
        print(f"  [FAIL] {label} {detail}")


def http(method, path, body=None, token=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.loads(resp.read().decode())


class WSClient:
    """最小 WebSocket 客户端：握手 + 文本帧收发（够测试用）。"""

    def __init__(self, token):
        hostport = BASE.split("://", 1)[1]
        host, port = hostport.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        path = "/ws?" + urlencode({"token": token})
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            f"Upgrade: websocket\r\n"
            f"Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            resp += self.sock.recv(4096)
        status = resp.split(b"\r\n", 1)[0]
        check(b"101" in status, f"WS 握手 {status.decode(errors='ignore').strip()}")
        self.buf = resp.split(b"\r\n\r\n", 1)[1]
        self.messages = []

    def _read_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("socket closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, t, payload=None, rid=None):
        msg = {"t": t, "payload": payload or {}}
        if rid:
            msg["rid"] = rid
        raw = json.dumps(msg).encode()
        mask = os.urandom(4)
        n = len(raw)
        header = bytearray([0x81])
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack("!H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack("!Q", n)
        header += mask
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(raw))
        self.sock.sendall(bytes(header) + masked)

    def recv(self, timeout=8):
        """返回一条解析后的消息，忽略 ping/pong 控制帧。"""
        self.sock.settimeout(timeout)
        deadline = time.time() + timeout
        while time.time() < deadline:
            h = self._read_exact(2)
            opcode = h[0] & 0x0F
            masked = h[1] & 0x80
            length = h[1] & 0x7F
            if length == 126:
                length = struct.unpack("!H", self._read_exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._read_exact(8))[0]
            mask = self._read_exact(4) if masked else None
            data = self._read_exact(length)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            if opcode == 0x1:  # text
                msg = json.loads(data.decode())
                self.messages.append(msg)
                return msg
            if opcode == 0x8:  # close
                raise EOFError("server closed")
            # ping/pong/其它 跳过
        raise TimeoutError("recv timeout")

    def wait_for(self, t, timeout=8, predicate=None):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = self.recv(timeout=max(0.5, deadline - time.time()))
            except (TimeoutError, socket.timeout, EOFError):
                break
            if msg.get("t") == t and (predicate is None or predicate(msg)):
                return msg
        return None

    def close(self):
        try:
            self.sock.close()
        except Exception:
            pass


def main():
    print(f"== game-gateway smoke test @ {BASE} ==")

    print("\n[1] REST 健康检查")
    r = http("GET", "/health")
    check(r.get("ok") and r.get("status") == "healthy", "health 返回 healthy", r)

    print("\n[2] 签发两个 dev token")
    ta = http("POST", "/api/v1/auth/dev-token", {"identity": "smoke_a", "nick": "Alice", "game": GAME})
    tb = http("POST", "/api/v1/auth/dev-token", {"identity": "smoke_b", "nick": "Bob", "game": GAME})
    token_a, token_b = ta.get("token"), tb.get("token")
    check(bool(token_a) and bool(token_b), "拿到两个 token")

    print("\n[3] REST 创建房间")
    room_resp = http("POST", "/api/v1/rooms", {
        "name": "smoke room", "game": GAME, "visibility": "public",
        "max_players": 2, "metadata": {"map": "test"},
    }, token=token_a)
    room = room_resp.get("room", {})
    room_id = room.get("id")
    check(bool(room_id), f"房间创建成功 id={room_id}", room_resp)
    check(room.get("host") == "smoke_a", "房主为 smoke_a")

    print("\n[4] 两个 WS 客户端连接并加入房间")
    a = WSClient(token_a)
    b = WSClient(token_b)
    check(a.wait_for("welcome") is not None, "A 收到 welcome")
    check(b.wait_for("welcome") is not None, "B 收到 welcome")

    a.send("room_join", {"room": room_id}, rid="r1")
    ja = a.wait_for("joined", predicate=lambda m: m.get("rid") == "r1")
    check(ja is not None, "A 加入房间")

    b.send("room_join", {"room": room_id}, rid="r2")
    jb = b.wait_for("joined", predicate=lambda m: m.get("rid") == "r2")
    check(jb is not None, "B 加入房间")
    check(a.wait_for("peer_joined", predicate=lambda m: m.get("payload", {}).get("room") == room_id) is not None,
          "A 收到 peer_joined（B 进房）")

    print("\n[5] 游戏事件透传 A -> B")
    marker = "hello_" + str(int(time.time()))
    a.send("game", {"event": "move", "data": {"row": 1, "col": 2, "marker": marker}})
    ev = b.wait_for("game", predicate=lambda m: m.get("payload", {}).get("event") == "move")
    check(ev is not None, "B 收到 game 事件")
    if ev:
        p = ev["payload"]
        check(p.get("from") == "smoke_a", "事件 from 正确")
        check(p.get("data", {}).get("marker") == marker, "事件 data 原样透传")

    print("\n[6] ready + 房主开局")
    b.send("ready", {"value": True}, rid="r3")
    ru = a.wait_for("room_update", predicate=lambda m: m.get("payload", {}).get("reason") == "ready")
    check(ru is not None, "A 收到 B 的 ready 更新")

    a.send("room_start", {}, rid="r4")
    rs = b.wait_for("room_state", predicate=lambda m: m.get("payload", {}).get("state") == "playing")
    check(rs is not None, "B 收到开局 room_state=playing")

    print("\n[7] REST 查询房间/在线玩家")
    detail = http("GET", f"/api/v1/rooms/{room_id}")
    check(detail.get("room", {}).get("state") == "playing", "REST 房间状态为 playing")
    players = http("GET", "/api/v1/players", token=token_a)
    check("smoke_a" in players.get("players", {}), "在线玩家包含 smoke_a")

    a.close()
    b.close()

    print(f"\n== 结果: {PASS} passed, {FAIL} failed ==")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n[ERROR] {type(e).__name__}: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
