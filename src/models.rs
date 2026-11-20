//! 领域模型与线协议：REST 与 WS 共用。
//! 设计要点：服务端不解释具体游戏逻辑，游戏事件在客户端之间原样透传。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type RoomId = String;
pub type PlayerId = String;

/// JWT 载荷。identity 是业务方自己的用户 ID，服务端不负责注册登录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// 业务用户 ID
    pub sub: String,
    /// 昵称（可选展示用）
    #[serde(default)]
    pub nick: String,
    /// 房间/对局所属的游戏标识，用于多游戏复用一套后端
    #[serde(default = "default_game")]
    pub game: String,
    pub exp: i64,
    pub iat: i64,
}

fn default_game() -> String {
    "default".to_string()
}

impl Claims {
    pub fn new(sub: impl Into<String>, nick: impl Into<String>, game: impl Into<String>, ttl: i64) -> Self {
        let now = Utc::now().timestamp();
        Self {
            sub: sub.into(),
            nick: nick.into(),
            game: game.into(),
            iat: now,
            exp: now + ttl,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RoomVisibility {
    Public,
    Private,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RoomState {
    #[default]
    Waiting,
    Playing,
    Finished,
}

/// 房间内的一个席位（按身份 ID 去重）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seat {
    pub identity: PlayerId,
    pub nick: String,
    pub ready: bool,
    pub is_host: bool,
    pub connected: bool,
}

/// 房间可见信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: RoomId,
    pub game: String,
    pub name: String,
    pub visibility: RoomVisibility,
    pub state: RoomState,
    pub max_players: usize,
    pub host: PlayerId,
    /// 自定义元数据，业务方自行塞入（如地图、模式、版本号）
    #[serde(default)]
    pub metadata: Value,
    pub seats: Vec<Seat>,
    pub created_at: DateTime<Utc>,
    /// 是否设置了加入密码（供客户端展示"需要密码"标志）；不泄露密码本身
    #[serde(default)]
    pub has_password: bool,
    /// 加入密码的 SHA-256 加盐哈希。绝不序列化给客户端。
    #[serde(skip_serializing, default)]
    pub password_hash: Option<String>,
    /// 被封禁的身份 ID 列表。绝不序列化给客户端（避免泄露玩家隐私）。
    #[serde(skip_serializing, default)]
    pub banned: Vec<PlayerId>,
}

/// —— WebSocket 线协议 ——
/// 客户端 → 服务端：`{ "t": "...", "payload": {...} }`
/// 服务端 → 客户端：`{ "t": "...", "payload": {...} }`
/// `t` = type，`payload` 全部是 JSON Value，后端只做路由/广播，不解释业务字段。

#[derive(Debug, Clone, Deserialize)]
pub struct ClientMessage {
    pub t: String,
    #[serde(default)]
    pub payload: Value,
    /// 客户端请求 id，服务端会在对应响应中原样带回，便于客户端匹配
    #[serde(default)]
    pub rid: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerMessage {
    pub t: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
}

impl ServerMessage {
    pub fn new(t: impl Into<String>, payload: Value) -> Self {
        Self {
            t: t.into(),
            payload,
            rid: None,
        }
    }

    pub fn with_rid(mut self, rid: Option<String>) -> Self {
        self.rid = rid;
        self
    }

    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 广播给房间内其他人时的信封：带上发送者身份，接收方才知道是谁发的。
#[derive(Debug, Clone, Serialize)]
pub struct RelayedEvent {
    pub from: PlayerId,
    pub nick: String,
    pub event: String,
    pub data: Value,
    pub room: RoomId,
    pub ts: DateTime<Utc>,
}

pub fn new_room_id() -> RoomId {
    Uuid::new_v4().to_string()
}
