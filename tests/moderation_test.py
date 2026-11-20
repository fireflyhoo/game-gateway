#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
房间管理功能测试：密码加入、踢人、封禁、解封、越权校验。仅用标准库。

验证链路：
  密码房创建 -> has_password=true 且不泄露密码 -> 错密码被拒(BAD_PASSWORD)
  -> 对密码加入 -> 第三人对密码加入 -> 房主踢第三人(kicked) -> 第三人可重新加入
  -> 房主封禁第三人(banned) -> 第三人再加入被拒(BANNED) -> 解封 -> 可再加入
  -> 非房主尝试踢人被拒(NOT_HOST)
"""
import base64
import json
import os
import socket
import struct
import sys
import time
import urllib.request
from urllib.parse import urlencode

BASE = os.environ.get("GATEWAY", "http://127.0.0.1:8080").rstrip("/")
GAME = "mod_" + str(int(time.time()))
PWD = "s3cr3t-room"
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


def token(identity, nick):
    r = http("POST", "/api/v1/auth/dev-token", {"identity": identity, "nick": nick, "game": GAME})
    return r["token"]


class WS:
    def __init__(self, tk):
        hp = BASE.split("://", 1)[1]
        host, port = hp.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        path = "/ws?" + urlencode({"token": tk})
        req = (f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
               f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            resp += self.sock.recv(4096)
        check(b"101" in resp.split(b"\r\n", 1)[0], "WS 握手 101")
        self.buf = resp.split(b"\r\n\r\n", 1)[1]

    def _exact(self, n):
        while len(self.buf) < n:
            c = self.sock.recv(65536)
            if not c:
                raise EOFError
            self.buf += c
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, t, payload=None, rid=None):
        m = {"t": t, "payload": payload or {}}
        if rid:
            m["rid"] = rid
        raw = json.dumps(m).encode()
        mask = os.urandom(4)
        n = len(raw)
        h = bytearray([0x81])
        if n < 126:
            h.append(0x80 | n)
        elif n < 65536:
            h.append(0x80 | 126); h += struct.pack("!H", n)
        else:
            h.append(0x80 | 127); h += struct.pack("!Q", n)
        h += mask
        self.sock.sendall(bytes(h) + bytes(b ^ mask[i % 4] for i, b in enumerate(raw)))

    def recv(self, timeout=8):
        self.sock.settimeout(timeout)
        deadline = time.time() + timeout
        while time.time() < deadline:
            h = self._exact(2)
            op = h[0] & 0x0F
            masked = h[1] & 0x80
            ln = h[1] & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", self._exact(2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", self._exact(8))[0]
            mask = self._exact(4) if masked else None
            data = self._exact(ln)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            if op == 0x1:
                return json.loads(data.decode())
            if op == 0x8:
                raise EOFError("closed")
        raise TimeoutError

    def wait(self, t, timeout=8, pred=None):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                m = self.recv(timeout=max(0.3, deadline - time.time()))
            except (TimeoutError, socket.timeout, EOFError):
                break
            if m.get("t") == t and (pred is None or pred(m)):
                return m
        return None

    def drain(self, seconds=0.3):
        """丢弃短期内到达的消息（清缓冲）"""
        self.sock.settimeout(seconds)
        try:
            while True:
                self.recv(timeout=seconds)
        except Exception:
            pass

    def close(self):
        try: self.sock.close()
        except Exception: pass


def main():
    print(f"== moderation test @ {BASE} ==")

    ta, tb, tc = token("mod_host", "房主"), token("mod_b", "老二"), token("mod_c", "老三")

    print("\n[1] REST 创建带密码房间")
    r = http("POST", "/api/v1/rooms", {
        "name": "密码房", "game": GAME, "password": PWD, "max_players": 4,
    }, token=ta)
    room = r.get("room", {})
    rid = room.get("id")
    check(bool(rid), f"房间创建 id={rid}", r)
    check(room.get("has_password") is True, "has_password=true")
    # 密码与哈希绝不外泄
    blob = json.dumps(room, ensure_ascii=False)
    check(PWD not in blob and "password_hash" not in blob and "banned" not in blob,
          "Room 响应不含明文密码/哈希/封禁列表", blob[:200])

    a, b, c = WS(ta), WS(tb), WS(tc)
    a.wait("welcome"); b.wait("welcome"); c.wait("welcome")

    print("\n[2] 错误密码被拒")
    b.send("room_join", {"room": rid, "password": "wrong"}, rid="bad")
    err = b.wait("error", pred=lambda m: m.get("rid") == "bad")
    check(err is not None and err["payload"].get("code") == "BAD_PASSWORD",
          "错误密码 -> BAD_PASSWORD", err)

    print("\n[3] 正确密码加入")
    b.send("room_join", {"room": rid, "password": PWD}, rid="okb")
    check(b.wait("joined", pred=lambda m: m.get("rid") == "okb") is not None, "老二对密码加入")
    c.send("room_join", {"room": rid, "password": PWD}, rid="okc")
    check(c.wait("joined", pred=lambda m: m.get("rid") == "okc") is not None, "老三对密码加入")
    a.drain(0.3)

    print("\n[4] 房主踢人")
    a.send("room_kick", {"target": "mod_c", "room": rid}, rid="k1")
    check(a.wait("kicked_ok", pred=lambda m: m.get("rid") == "k1") is not None, "房主收到 kicked_ok")
    check(c.wait("kicked", pred=lambda m: m.get("payload", {}).get("room") == rid) is not None,
          "老三收到 kicked")

    print("\n[5] 被踢者可重新加入（未封禁）")
    c.send("room_join", {"room": rid, "password": PWD}, rid="rejoin")
    check(c.wait("joined", pred=lambda m: m.get("rid") == "rejoin") is not None, "老三重新加入成功")
    a.drain(0.3); b.drain(0.3)

    print("\n[6] 非房主尝试踢人被拒")
    b.send("room_kick", {"target": "mod_c", "room": rid}, rid="noauth")
    e = b.wait("error", pred=lambda m: m.get("rid") == "noauth")
    check(e is not None and e["payload"].get("code") == "NOT_HOST", "非房主 -> NOT_HOST", e)

    print("\n[7] 房主封禁")
    a.send("room_ban", {"target": "mod_c", "room": rid}, rid="b1")
    check(a.wait("banned_ok", pred=lambda m: m.get("rid") == "b1") is not None, "房主收到 banned_ok")
    check(c.wait("banned", pred=lambda m: m.get("payload", {}).get("room") == rid) is not None,
          "老三收到 banned")

    print("\n[8] 封禁后拒绝加入")
    c.drain(0.3)
    c.send("room_join", {"room": rid, "password": PWD}, rid="blocked")
    e = c.wait("error", pred=lambda m: m.get("rid") == "blocked")
    check(e is not None and e["payload"].get("code") == "BANNED", "封禁后加入 -> BANNED", e)

    print("\n[9] 解封后可加入")
    a.send("room_unban", {"target": "mod_c", "room": rid}, rid="u1")
    check(a.wait("unbanned_ok", pred=lambda m: m.get("rid") == "u1") is not None, "房主收到 unbanned_ok")
    c.send("room_join", {"room": rid, "password": PWD}, rid="after_unban")
    check(c.wait("joined", pred=lambda m: m.get("rid") == "after_unban") is not None, "解封后重新加入成功")

    print("\n[10] REST 详情确认封禁列表不外泄")
    detail = http("GET", f"/api/v1/rooms/{rid}")
    check("banned" not in json.dumps(detail), "房间详情不含 banned 字段")

    a.close(); b.close(); c.close()
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
