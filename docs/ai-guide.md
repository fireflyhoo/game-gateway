# game-gateway 接入指南（供 AI 客户端阅读）

> 这是一份自包含的技术接入文档。读完本文与 `/ai/manifest.json`，你应能独立编写本服务的客户端代码，无需人类补充信息。

## 0. 这是什么

Web 联机游戏通用后端，一个二进制同时提供：

- **REST API**：健康检查、服务信息、房间增删改查、在线玩家
- **WebSocket**：实时通道，房间管理、准备/开局、游戏事件透传、私聊、聊天、心跳

**核心心智模型**：服务端只管"谁在哪个房间"和"把消息转给谁"，**不解释任何游戏玩法**。你的游戏逻辑通过 `game` 事件的 `data` 字段承载，服务端原样转发。一套部署可服务多个游戏，用 JWT 的 `game` 字段隔离。

## 1. 发现流程（AI 应先做这几步）

1. `GET /api/v1/info` — 拿到服务版本、在线统计、发现指针
2. `GET /ai/capabilities` — 拿到本实例实际开关与限制（如 dev-token 是否启用）
3. `GET /ai/manifest.json` — 完整机器可读能力清单（端点/消息/模型/错误码）
4. `GET /ai/openapi.json` — 标准 OpenAPI 3.1（REST）
5. `GET /llms.txt` — llmstxt 约定入口

下面所有 `<BASE>` 指服务根地址，如 `http://127.0.0.1:8080`。

## 2. 拿到 JWT

**生产**：你的登录服务用同一 `JWT_SECRET`（HS256）签发，claims：

```json
{ "sub": "u-123", "nick": "Alice", "game": "chess", "iat": 1700000000, "exp": 1700007200 }
```

`sub` 是用户唯一 ID，`game` 是游戏标识（房间隔离用），`iat`/`exp` 是 Unix 秒。

**联调**（仅当服务用默认 JWT_SECRET）：

```bash
curl -X POST <BASE>/api/v1/auth/dev-token \
  -H 'Content-Type: application/json' \
  -d '{"identity":"p1","nick":"Alice","game":"chess"}'
# => {"ok":true,"token":"eyJ...","token_type":"Bearer","expires_in":7200,...}
```

若返回 403 `DEV_TOKEN_DISABLED`，说明服务端已配自定义密钥，必须走业务方签发。

## 3. 建立 WebSocket 连接

浏览器 WS 不能自定义 Header，所以 token 走 query：

```js
const token = /* 上一步拿到的 JWT */;
const ws = new WebSocket(`${BASE}/ws?token=${encodeURIComponent(token)}`);
```

连上后服务端**首先**发 `welcome`：

```json
{ "t": "welcome", "payload": { "identity": "p1", "nick": "Alice", "game": "chess", "conn": "uuid", "rooms": [] } }
```

随后收到 `room_list`（当前公开房间）。

## 4. 统一消息格式

所有消息（双向）结构一致：

```json
{ "t": "消息类型", "payload": { ... }, "rid": "可选请求id" }
```

`rid` 由客户端填，服务端在对应响应里原样带回，用于并发匹配。**广播类消息不带 rid**。

## 5. 房间流程

### 创建房间

方式一，REST（需 Bearer token）：

```bash
curl -X POST <BASE>/api/v1/rooms \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"新手房","game":"chess","visibility":"public","max_players":2,"metadata":{"map":"desert"}}'
# => {"ok":true,"room":{...}}
```

方式二，WS：

```js
ws.send(JSON.stringify({ t: "room_create", payload: { name: "新手房", max_players: 2 }, rid: "c1" }));
// 收到 room_joined -> room_created -> room_list
```

`body.game` 必须等于 token 的 `game`，否则 400。

### 加密码的房间

创建时带 `password`（REST body 或 WS `room_create` payload 均可）：

```bash
curl -X POST <BASE>/api/v1/rooms -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"密码房","game":"chess","password":"1234"}'
```

服务端只存加盐哈希（`SHA256("game-gateway:room-password:v1:" + room_id + ":" + password)`）。任何房间的 JSON 响应里**只有 `has_password: true`**，密码和哈希永不返回——所以客户端必须先向玩家问密码，再加入。

加入时带密码：

```js
ws.send(JSON.stringify({ t: "room_join", payload: { room: roomId, password: "1234" }, rid: "j1" }));
// 密码错误 -> error(BAD_PASSWORD)；被封禁 -> error(BANNED)；满员 -> error(JOIN_CONFLICT)
```

密码校验失败**不会**把你从当前房间踢出去（先校验后切换）。

### 加入房间

```js
ws.send(JSON.stringify({ t: "room_join", payload: { room: roomId }, rid: "j1" }));
// 自己收到 joined（带 rid=J1）与 room_joined；房内其他人收到 peer_joined
```

### 踢人 / 封禁 / 解封（房主专用）

仅 `room.host == 你的 identity` 可执行，服务端强制校验，非房主返回 `error(NOT_HOST)`。

```js
// 踢人：对方收到 kicked，立即失去本房间广播，但之后可重新加入
ws.send(JSON.stringify({ t: "room_kick", payload: { target: "u-456", reason: "违规", room: roomId }, rid: "k1" }));
// -> 自己收到 kicked_ok；房内其他人收到 peer_kicked；目标收到 kicked

// 封禁：踢出 + 拉黑，目标无法再加入，直到解封
ws.send(JSON.stringify({ t: "room_ban", payload: { target: "u-456", room: roomId }, rid: "b1" }));
// -> banned_ok / peer_banned / 目标收到 banned

// 解封：目标可重新加入
ws.send(JSON.stringify({ t: "room_unban", payload: { target: "u-456", room: roomId }, rid: "u1" }));
// -> unbanned_ok / peer_unbanned / 目标收到 unbanned
```

`room` 参数可省略（用你当前所在房间）；但**若房间是 REST 创建的、你的 WS 还没 `room_join`，必须显式带 `room`**。稳妥做法是所有管理操作都带上 `roomId`。

- `target` 是对方的 JWT `sub`（identity），可从 `room.seats[].identity` 或 `players` 接口获得。
- 不能踢/封禁自己（`error(BAD_REQUEST)`）；重复封禁返回 `error(MODERATION_FAILED)`。
- 封禁列表不对外暴露（Room JSON 里没有 `banned` 字段），只有房主能通过操作结果感知。
- 房间解散后封禁列表随之消失（内存态）。

### 准备与开局

```js
ws.send(JSON.stringify({ t: "ready", payload: { value: true } }));   // 全员收到 room_update
// 房主：
ws.send(JSON.stringify({ t: "room_start", payload: {} }));           // 全员收到 room_state(state=playing)
```

`room_start` / `room_finish` 仅房主可用，否则 error `NOT_HOST`。

## 6. 游戏事件透传（重点）

这是联机玩法的核心。发：

```js
ws.send(JSON.stringify({
  t: "game",
  payload: { event: "move", data: { row: 1, col: 2, piece: "X" } }
}));
```

房间内**其他人**收到：

```json
{
  "t": "game",
  "payload": {
    "from": "p1", "nick": "Alice", "event": "move",
    "data": { "row": 1, "col": 2, "piece": "X" },
    "room": "房间uuid", "ts": "2026-11-20T04:00:00Z"
  }
}
```

要点：

- `data` 任意 JSON，服务端**不解析、不校验、不修改**，原样转发。
- 事件不回发给发送者（发送者本地已执行）。
- 私发：payload 加 `"to": "目标identity"`，只发给该身份；回执 `sent {to, delivered}`。

你可以用 `event` 自定义任意协议：`move`、`sync_state`、`chat_emote`、`frame_input`……帧同步、状态同步、锁步都基于此。

### 示例：井字棋落子同步

```js
ws.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.t === "game" && msg.payload.event === "move") {
    const { row, col, piece } = msg.payload.data;
    board[row][col] = piece;
    render();
  }
};

function play(row, col) {
  board[row][col] = myPiece;      // 本地先执行
  render();
  ws.send(JSON.stringify({ t: "game", payload: { event: "move", data: { row, col, piece: myPiece } } }));
}
```

## 7. 心跳与断线

- 服务端每 20s 发 RFC6455 Ping，浏览器自动回 Pong。
- 若 `WS_IDLE_TIMEOUT_SECS`（默认 120s）内服务端没收到你的任何消息，会发 `error(IDLE_TIMEOUT)` 后断开。
- 建议客户端定期发 `{ "t": "ping", "payload": {} }`，回 `pong {ts}`。
- 断线后重新握手即可；房间席位会在断开时移除（房主会移交）。

## 8. 完整消息速查

客户端可发：`ping` / `room_list` / `players` / `room_create` / `room_join` / `room_leave` / `room_kick` / `room_ban` / `room_unban` / `ready` / `room_start` / `room_finish` / `game` / `chat`

服务端会发：`welcome` / `pong` / `room_list` / `players` / `room_created` / `room_joined` / `joined` / `peer_joined` / `peer_left` / `room_update` / `room_state` / `game` / `chat` / `kicked` / `banned` / `unbanned` / `peer_kicked` / `peer_banned` / `peer_unbanned` / `kicked_ok` / `banned_ok` / `unbanned_ok` / `sent` / `left` / `room_dissolved` / `force_leave_room` / `error`

每个消息的 payload 字段详见 `/ai/manifest.json` 的 `websocket.client_messages` 与 `websocket.server_messages`。

## 9. 错误码

REST（HTTP 状态码）：400 BAD_REQUEST / 401 UNAUTHORIZED / 403 DEV_TOKEN_DISABLED / 404 NOT_FOUND / 409 CONFLICT / 500 INTERNAL

WS（`payload.code`）：BAD_JSON / IDLE_TIMEOUT / BAD_NAME / MISSING_ROOM / NO_ROOM / GAME_MISMATCH / JOIN_FAILED / JOIN_CONFLICT / BAD_PASSWORD / BANNED / MISSING_TARGET / NOT_FOUND / NOT_HOST / BAD_REQUEST / MODERATION_FAILED / NOT_IN_ROOM / READY_FAILED / NO_EVENT / EMPTY / UNKNOWN_TYPE / MISSING_TOKEN

## 10. 生产注意事项

1. 改掉默认 `JWT_SECRET`，dev-token 接口会自动关闭。
2. 用 Nginx/Caddy 反代终止 TLS，对外提供 HTTPS/WSS。
3. 本服务内存态、不持久化对局结果；结算由你的业务服务落库。
4. 多实例部署：把 `src/state.rs` 的 DashMap 换成 Redis（房间 Hash + Pub/Sub 广播）。
5. 不要把 token / JWT_SECRET 写进日志或前端硬编码。
