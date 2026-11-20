//! REST API：健康检查、服务信息、房间管理、在线玩家。
//! 所有写操作（创建/删除房间）都需要 Bearer token。

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{dev_sign, AuthUser};
use crate::docs::doc_resources;
use crate::error::{AppError, AppResult};
use crate::models::{RoomVisibility, Seat};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/info", get(info))
        .route("/api/v1/auth/dev-token", post(dev_token))
        .route("/api/v1/rooms", get(list_rooms).post(create_room))
        .route("/api/v1/rooms/{room_id}", get(get_room).delete(delete_room))
        .route("/api/v1/players", get(list_players))
}

// ——————————————————————————— handlers ———————————————————————————

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "status": "healthy",
        "time": Utc::now().to_rfc3339(),
    }))
}

async fn info(State(state): State<AppState>) -> Json<Value> {
    let rooms = state.list_rooms(None, None);
    Json(json!({
        "ok": true,
        "service": "game-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "stats": {
            "rooms": rooms.len(),
            "connections": state.connections.len(),
            "online_identities": state.online_players(None).len(),
        },
        "limits": {
            "max_players_default": state.config.max_players_default,
            "ws_idle_timeout_secs": state.config.ws_idle_timeout_secs,
        },
        "discovery": {
            "note": "本服务为 AI 客户端提供自描述文档。",
            "docs": doc_resources(),
        }
    }))
}

#[derive(Debug, Deserialize)]
struct DevTokenReq {
    identity: String,
    #[serde(default)]
    nick: String,
    #[serde(default = "default_game")]
    game: String,
}

fn default_game() -> String {
    "default".to_string()
}

/// 仅当使用默认 JWT_SECRET 时可用；方便本地/联调一键拿 token。
async fn dev_token(
    State(state): State<AppState>,
    Json(body): Json<DevTokenReq>,
) -> AppResult<Json<Value>> {
    let identity = body.identity.trim();
    if identity.is_empty() {
        return Err(AppError::BadRequest("identity 不能为空".to_string()));
    }
    let token = dev_sign(&state, identity, &body.nick, &body.game)?;
    Ok(Json(json!({
        "ok": true,
        "token": token,
        "token_type": "Bearer",
        "expires_in": state.config.jwt_ttl_secs,
        "identity": identity,
        "game": body.game,
    })))
}

#[derive(Debug, Deserialize)]
struct RoomListQuery {
    game: Option<String>,
    visibility: Option<String>,
}

async fn list_rooms(
    State(state): State<AppState>,
    Query(query): Query<RoomListQuery>,
) -> Json<Value> {
    let visibility = match query.visibility.as_deref() {
        Some("public") => Some(RoomVisibility::Public),
        Some("private") => Some(RoomVisibility::Private),
        _ => None,
    };
    let rooms = state.list_rooms(query.game.as_deref(), visibility);
    Json(json!({ "ok": true, "rooms": rooms, "count": rooms.len() }))
}

#[derive(Debug, Deserialize)]
struct CreateRoomReq {
    name: String,
    #[serde(default = "default_game")]
    game: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    max_players: Option<usize>,
    #[serde(default)]
    metadata: Value,
    /// 加入密码（可选）；仅存哈希，不对外泄露
    #[serde(default)]
    password: Option<String>,
}

async fn create_room(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateRoomReq>,
) -> AppResult<Json<Value>> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::BadRequest("房间名需为 1~64 个字符".to_string()));
    }
    if body.game.is_empty() || body.game.len() > 64 {
        return Err(AppError::BadRequest("game 标识需为 1~64 个字符".to_string()));
    }

    let visibility = match body.visibility.as_deref() {
        Some("private") => RoomVisibility::Private,
        _ => RoomVisibility::Public,
    };
    let max_players = body
        .max_players
        .unwrap_or(state.config.max_players_default)
        .clamp(2, 64);

    // token 里的 game 必须和房间 game 一致（防止跨游戏串房）
    if auth.0.game != body.game {
        return Err(AppError::BadRequest(
            "token 中的 game 与请求的 game 不一致".to_string(),
        ));
    }

    let nick = if auth.0.nick.is_empty() {
        auth.0.sub.clone()
    } else {
        auth.0.nick.clone()
    };
    let room = state.create_room(
        &body.game,
        name.to_string(),
        max_players,
        visibility,
        body.metadata,
        auth.0.sub.clone(),
        nick,
        body.password.as_deref(),
    );

    if room.visibility == RoomVisibility::Public {
        state.broadcast_room_list(&body.game);
    }
    Ok(Json(json!({ "ok": true, "room": room })))
}

async fn get_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> AppResult<Json<Value>> {
    let room = state.get_room(&room_id)?;
    Ok(Json(json!({ "ok": true, "room": room })))
}

async fn delete_room(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(room_id): Path<String>,
) -> AppResult<Json<Value>> {
    let room = state.get_room(&room_id)?;
    if room.host != auth.0.sub {
        return Err(AppError::Conflict("只有房主可以解散房间".to_string()));
    }

    // 通知房间内所有人
    let msg = serde_json::json!({
        "t": "room_dissolved",
        "payload": { "room": room_id, "by": auth.0.sub }
    })
    .to_string();
    state.broadcast_room(&room, &msg, None);

    // 断开房间内所有连接的 room 归属
    for conn in state.connections.iter() {
        if conn.value().room.as_deref() == Some(room_id.as_str()) {
            // 通过 sender 发一个 forced_leave，Handler 会据此清理本地状态
            let _ = conn.value().sender.send(
                axum::extract::ws::Message::Text(
                    serde_json::json!({
                        "t": "force_leave_room",
                        "payload": { "room": room_id }
                    })
                    .to_string()
                    .into(),
                ),
            );
        }
    }

    state.rooms.remove(&room_id);
    state.broadcast_room_list(&room.game);
    Ok(Json(json!({ "ok": true, "dissolved": room_id })))
}

async fn list_players(
    State(state): State<AppState>,
    Query(query): Query<RoomListQuery>,
) -> Json<Value> {
    let players = state.online_players(query.game.as_deref());
    Json(json!({ "ok": true, "players": players, "count": players.len() }))
}

/// 保留导出，WS 模块会用到 Seat 的构造（避免 dead_code 警告）
#[allow(dead_code)]
fn _seat_marker(_: Seat) {}
