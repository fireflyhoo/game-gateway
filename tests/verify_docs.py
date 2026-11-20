#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
自描述文档校验：确认 AI 端点可达、JSON 合法、占位符已替换、文档与源码路由一致。

用法：
  python tests/verify_docs.py
  GATEWAY=http://127.0.0.1:8080 python tests/verify_docs.py
"""
import json
import os
import re
import sys
import urllib.request

BASE = os.environ.get("GATEWAY", "http://127.0.0.1:8080").rstrip("/")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
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


def fetch(path):
    with urllib.request.urlopen(BASE + path, timeout=10) as resp:
        return resp.read().decode("utf-8"), resp.headers.get("Content-Type", "")


def fetch_json(path):
    raw, ctype = fetch(path)
    return json.loads(raw), ctype


def source_routes():
    """从 src/api.rs 与 src/main.rs 正则提取实际注册的 REST 路径。"""
    found = set()
    for filename in ("src/api.rs", "src/main.rs", "src/docs.rs"):
        with open(os.path.join(ROOT, filename), "r", encoding="utf-8") as f:
            src = f.read()
        for m in re.finditer(r'\.route\(\s*"([^"]+)"', src):
            found.add(m.group(1))
    return found


def declared_rest_paths(manifest):
    paths = set()
    for ep in manifest.get("rest", {}).get("endpoints", []):
        p = ep.get("path", "")
        # manifest 中 path 已是完整路径（如 /api/v1/rooms/{room_id}），直接使用
        if p.startswith("/"):
            paths.add(p)
    return paths


def main():
    print(f"== 自描述文档校验 @ {BASE} ==")

    print("\n[1] 根发现文档 /")
    root, ctype = fetch_json("/")
    check(root.get("ok") is True, "根文档 ok=true")
    check(root.get("service") == "game-gateway", "service 字段正确")
    check(isinstance(root.get("resources"), list) and len(root["resources"]) >= 7,
          "resources 列出全部文档端点", f"count={len(root.get('resources', []))}")
    check("start_here" in root, "含 start_here 引导")
    check("{{" not in json.dumps(root, ensure_ascii=False), "根文档无未替换占位符")

    print("\n[2] /api/v1/info 的 discovery 指针")
    info, _ = fetch_json("/api/v1/info")
    check(info.get("ok") is True, "info ok=true")
    docs = info.get("discovery", {}).get("docs", [])
    check(len(docs) >= 7, "info 含 discovery.docs", f"count={len(docs)}")

    print("\n[3] /llms.txt")
    llms, ctype = fetch("/llms.txt")
    check("text/plain" in ctype, "Content-Type 为 text/plain", ctype)
    check("game-gateway" in llms and "base_url:" in llms, "llms.txt 含服务名与 base_url")
    check("/ai/guide" in llms and "/ai/manifest.json" in llms, "llms.txt 列出关键文档")
    check("{{BASE_URL}}" not in llms, "llms.txt 占位符已替换")

    print("\n[4] /ai/manifest.json")
    manifest, _ = fetch_json("/ai/manifest.json")
    check(manifest.get("spec", "").startswith("game-gateway-ai-manifest"), "manifest spec 正确")
    check(manifest.get("generated_for_ai") is True, "标记 generated_for_ai")
    rest_eps = manifest.get("rest", {}).get("endpoints", [])
    ws_client = manifest.get("websocket", {}).get("client_messages", [])
    ws_server = manifest.get("websocket", {}).get("server_messages", [])
    check(len(rest_eps) >= 8, f"声明 {len(rest_eps)} 个 REST 端点")
    check(len(ws_client) >= 11, f"声明 {len(ws_client)} 种客户端 WS 消息")
    check(len(ws_server) >= 15, f"声明 {len(ws_server)} 种服务端 WS 消息")
    check("models" in manifest and "Room" in manifest["models"], "含数据模型定义")
    check("errors" in manifest, "含错误码定义")
    check(manifest.get("service", {}).get("base_url", "").startswith("http"),
          "base_url 已动态替换", manifest.get("service", {}).get("base_url", ""))
    check("{{" not in json.dumps(manifest, ensure_ascii=False), "manifest 无未替换占位符")
    # game 事件必须在清单中且标注为透传
    game_msgs = [m for m in ws_client if m.get("t") == "game"]
    check(bool(game_msgs) and "透传" in game_msgs[0].get("desc", ""), "game 事件标注为透传")
    # 管理操作消息必须声明
    client_types = {m.get("t") for m in ws_client}
    for need in ("room_kick", "room_ban", "room_unban"):
        check(need in client_types, f"manifest 声明 {need}")
    server_types = {m.get("t") for m in ws_server}
    for need in ("kicked", "banned", "unbanned", "peer_kicked", "peer_banned", "peer_unbanned"):
        check(need in server_types, f"manifest 声明服务端消息 {need}")
    # 密码参数必须声明
    join_msg = next((m for m in ws_client if m.get("t") == "room_join"), {})
    check("password" in json.dumps(join_msg.get("payload", {})), "room_join 声明 password 参数")
    create_body = next((e for e in rest_eps if e.get("name") == "create_room"), {}).get("request_body", {})
    check("password" in create_body, "REST create_room 声明 password 参数")
    # 模型必须声明 has_password 且不外泄密码
    room_model = json.dumps(manifest.get("models", {}).get("Room", {}), ensure_ascii=False)
    check("has_password" in room_model and "不序列化" in room_model, "Room 模型声明 has_password 与密码不外泄")

    print("\n[5] /ai/openapi.json")
    spec, _ = fetch_json("/ai/openapi.json")
    check(spec.get("openapi", "").startswith("3.1"), "OpenAPI 3.1", spec.get("openapi", ""))
    check("paths" in spec and len(spec["paths"]) >= 12, f"声明 {len(spec.get('paths', {}))} 条 path")
    check("x-websocket" in spec.get("info", {}), "info 含 x-websocket 扩展指向 WS 文档")
    check("securitySchemes" in spec.get("components", {}), "含 bearerAuth 定义")
    check("{{" not in json.dumps(spec, ensure_ascii=False), "openapi 无未替换占位符")

    print("\n[6] /ai/capabilities")
    cap, _ = fetch_json("/ai/capabilities")
    check(cap.get("ok") is True, "capabilities ok=true")
    check("features" in cap and cap["features"].get("websocket") is True, "features 含 websocket")
    check("limits" in cap and "max_players_range" in cap["limits"], "limits 含 max_players_range")
    check("runtime" in cap, "含 runtime 实时统计")
    check(isinstance(cap.get("features", {}).get("dev_token_endpoint"), bool),
          "dev_token_endpoint 为布尔值")

    print("\n[7] /ai/guide")
    guide, ctype = fetch("/ai/guide")
    check("text/markdown" in ctype, "Content-Type 为 text/markdown", ctype)
    check("# game-gateway" in guide and "game" in guide, "guide 含标题与内容")
    check("WebSocket" in guide and "JWT" in guide, "guide 含 WS 与鉴权说明")
    check("{{" not in guide, "guide 占位符已替换")

    print("\n[8] 文档与源码路由一致性")
    src = source_routes()
    declared = declared_rest_paths(manifest)
    # manifest 里 REST path 与源码 route 字符串直接对比（两者都用 {room_id} 形式）
    missing = sorted(p for p in declared if p not in src)
    check(not missing, "manifest 声明的 REST 路径均在源码注册", f"缺失: {missing}")
    # 反向：源码中 api/v1 路由应都在 manifest 声明（文档/根路由豁免）
    api_src = {p for p in src if p.startswith("/api/v1")}
    undocumented = sorted(p for p in api_src if p not in declared)
    check(not undocumented, "源码的 /api/v1 路由均在 manifest 声明", f"未文档化: {undocumented}")

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
