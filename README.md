# game-gateway

Web 联机游戏的**通用后端底座**。一套服务同时提供：

- **REST API**：健康检查、房间创建/查询/解散、在线玩家、鉴权
- **WebSocket**：实时通道，房间管理、准备/开局、游戏事件透传、私聊、心跳
- **AI 自描述文档**：服务运行时提供机器可读的能力清单，AI 客户端连上即可自助接入

服务端**不解释任何游戏玩法**——`game` 事件的 `data` 由客户端自定义并原样转发。一套部署可服务多个游戏（用 JWT 里的 `game` 字段隔离）。

技术栈：Rust 1.83+ / axum 0.8 / tokio / dashmap。单二进制，无外部中间件依赖。

---

## 1. 快速开始

### 本地运行

```bash
cp .env.example .env
# 开发模式直接用默认 JWT_SECRET，dev-token 接口可用
cargo run --release
```

服务监听 `http://127.0.0.1:8080`。

### Docker 部署

```bash
cp .env.example .env
# ★ 修改 .env 里的 JWT_SECRET 为高强度随机串
docker compose up -d --build
curl http://127.0.0.1:8080/health
```

> 生产环境请务必把 `JWT_SECRET` 改成随机串（如 `openssl rand -hex 32`），改完后 `dev-token` 接口自动关闭，改由你的业务服务签发 JWT。

---

## 2. AI 自描述文档（给 AI 客户端）

服务本身会"自我介绍"：AI 客户端访问几个固定端点，就能拿到全部 REST/WS 能力、数据模型、错误码和接入示例，**无需人类提供文档**。文档以标准格式编译期嵌入二进制（无外部文件依赖），响应时自动把占位符替换成实际访问地址（含反向代理场景）。

| 端点 | 格式 | 用途 |
|---|---|---|
| `/` | JSON | 根发现文档，列出所有自描述资源，建议入口 |
| `/api/v1/info` | JSON | 服务状态 + `discovery.docs` 发现指针 |
| `/llms.txt` | text/plain | llmstxt.org 约定的 AI 入口清单 |
| `/ai/manifest.json` | JSON | **核心**：全部端点、WS 消息类型与 payload、模型、错误码、典型流程 |
| `/ai/openapi.json` | JSON | OpenAPI 3.1 规范（REST），可直接导入 API 工具链 |
| `/ai/capabilities` | JSON | 当前实例实际开关与限制（dev-token 是否启用、上限、在线数） |
| `/ai/guide` | Markdown | 完整接入指南 + 可直接抄的代码示例 |

**AI 的建议阅读顺序**：

```
GET /  →  GET /api/v1/info  →  GET /ai/capabilities  →  GET /ai/manifest.json  →  GET /ai/guide
```

- 需要程序化生成客户端 → 读 `manifest.json`
- 需要导入标准 API 工具 → 读 `openapi.json`
- 需要理解玩法语义、看示例 → 读 `/ai/guide`

维护约定：`docs/` 下的文档资产与 `src/` 代码同源；新增/修改端点时同步更新 `docs/ai-manifest.json`、`docs/openapi.json`、`docs/ai-guide.md`，并运行 `python tests/verify_docs.py` 检查文档与源码路由一致性（防止漂移）。

---

## 3. 鉴权

两种接入方式：

**A. 业务方签发（推荐，生产）**

你的登录服务用同一个 `JWT_SECRET` 签发 HS256 token，载荷：

```json
{
  "sub": "用户唯一ID",
  "nick": "昵称",
  "game": "chess",
  "iat": 1700000000,
  "exp": 1700007200
}
```

**B. dev-token（仅联调）**

当 `JWT_SECRET` 为默认值时：

```bash
curl -X POST http://127.0.0.1:8080/api/v1/auth/dev-token \
  -H 'Content-Type: application/json' \
  -d '{"identity":"p1","nick":"Alice","game":"chess"}'
```

返回 `token`。REST 用 `Authorization: Bearer <token>`，WS 用 `?token=<token>`。

`game` 字段用于多游戏隔离：房间只对本 game 的玩家可见，跨 game 加入会被拒绝。

---

## 4. REST API

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/health` | 否 | 健康检查 |
| GET | `/api/v1/info` | 否 | 服务信息与在线统计 |
| POST | `/api/v1/auth/dev-token` | 否 | 开发 token（默认密钥时） |
| GET | `/api/v1/rooms?game=chess&visibility=public` | 否 | 房间列表 |
| POST | `/api/v1/rooms` | 是 | 创建房间 |
| GET | `/api/v1/rooms/{room_id}` | 否 | 房间详情 |
| DELETE | `/api/v1/rooms/{room_id}` | 是 | 解散房间（仅房主） |
| GET | `/api/v1/players?game=chess` | 否 | 在线玩家 |

机器可读版本见 `/ai/openapi.json` 与 `/ai/manifest.json`。

创建房间请求体（`password` 可选）：

```json
{
  "name": "新手房",
  "game": "chess",
  "visibility": "public",
  "max_players": 2,
  "metadata": { "map": "desert", "mode": "ranked" },
  "password": "1234"
}
```

> `password` 只以加盐哈希存储，房间返回体仅含 `has_password: true`，密码与哈希永不泄露给客户端。设了密码的房间加入时需在 WS `room_join` 带 `password`。

---

## 5. WebSocket 协议

连接：`ws://127.0.0.1:8080/ws?token=<jwt>`

所有消息统一结构：

```json
{ "t": "消息类型", "payload": { ... }, "rid": "可选请求id" }
```

带 `rid` 的消息，响应会原样带回 `rid`，方便客户端匹配请求/响应。

### 客户端 → 服务端

| `t` | payload | 说明 |
|---|---|---|
| `ping` | `{}` | 心跳，回 `pong` |
| `room_list` | `{}` | 请求房间列表 |
| `players` | `{}` | 请求在线玩家 |
| `room_create` | `{name, max_players?, visibility?, metadata?}` | 创建并自动加入 |
| `room_join` | `{room: "<room_id>", password?}` | 加入房间（加密房间需带 password；准备中/对局中/结算后均可加入） |
| `room_leave` | `{}` | 离开房间 |
| `room_kick` | `{target: "<identity>", room?, reason?}` | **房主**踢人 |
| `room_ban` | `{target: "<identity>", room?, reason?}` | **房主**封禁（不可再加入） |
| `room_unban` | `{target: "<identity>", room?}` | **房主**解封 |
| `ready` | `{value: true/false}` | 准备状态 |
| `room_start` | `{}` | 房主开局 |
| `room_finish` | `{}` | 房主结束 |
| `game` | `{event, data, to?}` | **游戏事件透传**；有 `to` 则私发，否则广播给房间其他人 |
| `state_save` | `{state: <不透明快照>}` | **房主**提交权威状态快照（≤512KB），回 `state_saved`；供断线接管 |
| `chat` | `{text}` | 房间聊天（不在房间则大厅广播） |

### 服务端 → 客户端

| `t` | payload | 触发时机 |
|---|---|---|
| `welcome` | `{identity, nick, game, conn, rooms}` | 连接成功 |
| `room_list` | `{rooms}` | 房间变更 / 请求后 |
| `players` | `{players}` | 请求后 |
| `room_joined` | `{room}` | 加入房间成功 |
| `joined` | `{room}` | `room_join` 的直接响应 |
| `peer_joined` | `{room, seat}` | 有人加入房间 |
| `peer_left` | `{room, identity, room_state?}` | 有人离开房间 |
| `room_update` | `{room, reason, seat?}` | ready / 离线等状态变更 |
| `room_state` | `{room, state}` | 开局 / 结束 |
| `game` | `{from, nick, event, data, room, ts}` | 收到游戏事件 |
| `state_saved` | `{room, bytes}` | `state_save` 成功回执 |
| `state_takeover` | `{room, new_host, from, owner, state, updated_at_ms}` | 房主断线且有快照时，推给新房主 |
| `chat` | `{from, nick, text, room, ts}` | 收到聊天 |
| `sent` | `{to, delivered}` | 私发回执 |
| `left` | `{}` | `room_leave` 的直接响应 |
| `kicked` / `banned` / `unbanned` | `{room, by, reason?}` | 本人被踢 / 被封 / 被解封 |
| `peer_kicked` / `peer_banned` / `peer_unbanned` | `{room, target, by, room_state}` | 房内其他人被踢/封/解封 |
| `kicked_ok` / `banned_ok` / `unbanned_ok` | `{target, room}` | 管理操作成功回执 |
| `room_dissolved` | `{room, by}` | 房主解散房间 |
| `force_leave_room` | `{room}` | 房间被解散，清本地状态 |
| `pong` | `{ts}` | ping 的响应 |
| `error` | `{code, message}` | 出错 |

### 房间管理：踢人 / 封禁 / 解封

仅**房主**（`room.host == 你的 identity`）可操作，服务端强制校验，非房主返回 `error(NOT_HOST)`。`target` 是对方的 identity（从 `room.seats[].identity` 或 `/api/v1/players` 拿）：

```js
ws.send(JSON.stringify({ t: 'room_kick',   payload: { target: 'u-456', room: roomId }, rid: 'k1' })); // 踢出，对方仍可再加入
ws.send(JSON.stringify({ t: 'room_ban',    payload: { target: 'u-456', room: roomId }, rid: 'b2' })); // 封禁，无法再加入直到解封
ws.send(JSON.stringify({ t: 'room_unban',  payload: { target: 'u-456', room: roomId }, rid: 'u3' })); // 解封
```

`room` 可省略（用当前所在房间），但 REST 建房的房主 WS 尚未入房，**需显式带 `room`**。

- 被踢/被封禁者立即失去该房间的广播，并收到 `kicked` / `banned`；房内其他人收到 `peer_kicked` / `peer_banned`。
- 封禁列表不下发给任何客户端（Room JSON 里没有 `banned` 字段）。不能踢/封自己。
- 加密码房间：创建时带 `password`，加入时 `room_join` 带 `password`；密码错误 → `BAD_PASSWORD`，被封禁 → `BANNED`，且不影响你当前所在房间。

### 游戏事件透传示例（以井字棋为例）

```js
const ws = new WebSocket('ws://127.0.0.1:8080/ws?token=' + token);

ws.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.t === 'game') {
    const { from, event, data } = msg.payload;
    if (event === 'move') applyMove(from, data.row, data.col);   // data 原样收到
    if (event === 'sync_state') loadState(data);
  }
};

// 玩家落子：服务端不关心 row/col 是什么，原样转发给房间其他人
function play(row, col) {
  ws.send(JSON.stringify({
    t: 'game',
    payload: { event: 'move', data: { row, col } }
  }));
}
```

想做帧同步 / 状态同步 / 锁步，都基于 `game` 事件自己定义 `event` 和 `data` 结构即可。更完整的接入示例见 `/ai/guide`。

---

## 6. 架构与扩展

```
浏览器 A ─┐
浏览器 B ─┼─ WS(/ws) ─┐
浏览器 C ─┘            │
                       ├─ AppState ── rooms(DashMap) ── 房间/席位/状态
业务服务 ── REST ──────┘            └─ connections(DashMap) ── 连接池
   │
   └─ 用同一 JWT_SECRET 签发 token
AI 客户端 ─ GET / , /ai/* ─ 自描述文档（编译期嵌入）
```

- **多实例横向扩展**：当前房间/连接是进程内 DashMap。要上多台机器，把 `RoomRegistry` 和 `ConnectionPool` 换成 Redis（房间态用 Hash，跨节点广播用 Pub/Sub），并在前面挂负载均衡 + sticky session 或纯 Pub/Sub 广播。接口已收敛在 `state.rs`，替换点集中。
- **WSS**：生产用 Nginx/Caddy 反代终止 TLS，后端只跑纯 WS。反代时透传 `X-Forwarded-Proto/Host`，自描述文档会据此生成正确的绝对地址。
- **对局状态持久化**：本服务是**通道**，不存对局结果。结算由业务服务落库。
- **房主权威与断线接管**：若游戏采用「房主跑唯一模拟、其他人只收状态」的架构，房主可用 `state_save` 定期把世界快照（网关不解释的不透明 blob）交给网关保管；房主断线移交时，网关把最后快照以 `state_takeover` 推给新房主，对方据此恢复世界续跑，避免整局中止。`room_start` 会清空旧快照，房间解散也会清理。快照上限约 512KB，仅房主可写。
- **观战 / 回放**：可基于 `game` 事件在客户端录制，或另接消息队列持久化。

---

## 7. 目录结构

```
game-gateway/
├── Cargo.toml
├── Dockerfile
├── docker-compose.yml
├── .env.example
├── docs/                       # AI 自描述文档资产（include_str! 嵌入）
│   ├── ai-manifest.json        # 机器可读能力清单（核心）
│   ├── openapi.json            # OpenAPI 3.1
│   ├── ai-guide.md             # 接入指南
│   └── llms.txt                # llms.txt 入口
├── src/
│   ├── main.rs       # 组装路由、CORS、优雅停机
│   ├── config.rs     # 环境变量配置
│   ├── error.rs      # 统一错误
│   ├── models.rs     # 领域模型 + 线协议
│   ├── state.rs      # 房间/连接共享状态（★替换 Redis 的切入点）
│   ├── auth.rs       # JWT 签发/校验 + AuthUser 提取器
│   ├── api.rs        # REST 路由
│   ├── ws.rs         # WebSocket 核心（★游戏事件透传在这里）
│   └── docs.rs       # 自描述文档端点（占位符替换、发现指针）
└── tests/
    ├── smoke_test.py       # 无第三方依赖的全链路冒烟测试
    ├── moderation_test.py  # 密码/踢人/封禁/解封测试
    └── verify_docs.py      # 自描述文档校验 + 文档/源码一致性
```

---

## 8. 测试

```bash
# 服务运行后，另开终端：
python tests/smoke_test.py       # REST + WS 全链路（18 项）
python tests/moderation_test.py  # 密码/踢人/封禁/解封/越权（20+ 项）
python tests/verify_docs.py      # AI 自描述端点 + 文档一致性（50+ 项）
# 或指定地址
GATEWAY=http://127.0.0.1:8080 python tests/moderation_test.py
```

`smoke_test.py` 验证：health → dev-token → 创建房间 → 两个 WS 客户端加入同一房间 → A 发 `game` 事件 → B 收到 → ready → 开局。

`moderation_test.py` 验证：密码房创建与 `has_password` 标志、密码/哈希/封禁列表不外泄、错误密码拒绝、对密码加入、踢人、被踢者重加、非房主越权拒绝、封禁后拒绝加入、解封后恢复。

`verify_docs.py` 验证：所有自描述端点可达、JSON 合法、占位符已替换、`game` 透传语义与管理消息已声明，且 manifest 中声明的 REST 路径与源码实际注册路由一致（防文档漂移）。
