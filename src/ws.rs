//! WebSocket 实时服务。
//! 连接：/ws?token=<jwt>
//! 客户端消息：{ "t": "<type>", "payload": {...}, "rid": "可选" }
//! 服务端消息：同结构。游戏自定义事件用 t="game",payload={event,data} 原样转发给房间其他人。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::auth::{token_from_query, verify};
use crate::error::AppError;
use crate::models::{ClientMessage, RelayedEvent, Room, RoomState, Seat, ServerMessage};
use crate::state::{AppState, Connection};

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: String,
}

/// 升级为 WebSocket 连接。token 在 query 里校验（浏览器 WS 无法自定义 Header）。
pub async fn handler(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let token = token_from_query(Some(&query.token))?;
    let claims = verify(&state.config.jwt_secret, &token)?;

    // 限制同一身份的并发连接数
    if state.connection_count(&claims.sub) >= state.config.max_connections_per_identity {
        return Err(AppError::Conflict("该身份连接数已达上限".to_string()));
    }

    Ok(ws.on_upgrade(move |socket| {
        let state = state.clone();
        async move { run_connection(socket, state, claims).await }
    }))
}

async fn run_connection(
    socket: WebSocket,
    state: AppState,
    claims: crate::models::Claims,
) {
    let conn_id = Uuid::new_v4().to_string();
    let identity = claims.sub.clone();
    let nick = if claims.nick.is_empty() {
        identity.clone()
    } else {
        claims.nick.clone()
    };
    let game = claims.game.clone();

    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // 写任务：所有出站消息统一从这里发给 socket
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 注册连接
    state.connections.insert(
        conn_id.clone(),
        Connection {
            identity: identity.clone(),
            nick: nick.clone(),
            game: game.clone(),
            room: None,
            sender: tx.clone(),
        },
    );

    info!(identity = %identity, conn = %conn_id, game = %game, "WS connected");

    // 欢迎包
    let welcome = ServerMessage::new(
        "welcome",
        json!({
            "identity": identity,
            "nick": nick,
            "game": game,
            "conn": conn_id,
            "rooms": state.list_rooms(Some(&game), None),
        }),
    );
    let _ = tx.send(Message::Text(welcome.to_text().into()));

    // 把房间列表推给同 game 的所有人（ himself 包括）
    state.broadcast_room_list(&game);

    let idle_timeout = Duration::from_secs(state.config.ws_idle_timeout_secs);
    let mut last_seen = Instant::now();

    // 周期性心跳探测：每 20s 发一次应用级 ping 帧
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.tick().await; // 立即消耗第一次

    loop {
        tokio::select! {
            // 客户端消息
            maybe_msg = source.next() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => {
                        last_seen = Instant::now();
                        let raw = text.as_ref();
                        match serde_json::from_str::<ClientMessage>(raw) {
                            Ok(msg) => {
                                if let Some(reply) = dispatch(
                                    &state, &conn_id, &identity, &nick, &game, msg, &tx,
                                ).await {
                                    let _ = tx.send(Message::Text(reply.to_text().into()));
                                }
                            }
                            Err(e) => {
                                let err = ServerMessage::new(
                                    "error",
                                    json!({ "code": "BAD_JSON", "message": e.to_string() }),
                                );
                                let _ = tx.send(Message::Text(err.to_text().into()));
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        last_seen = Instant::now();
                        let _ = tx.send(Message::Pong(data));
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_seen = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!(identity = %identity, "WS read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            // 心跳
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > idle_timeout {
                    info!(identity = %identity, "WS idle timeout");
                    let bye = ServerMessage::new("error", json!({
                        "code": "IDLE_TIMEOUT",
                        "message": "连接空闲超时"
                    }));
                    let _ = tx.send(Message::Text(bye.to_text().into()));
                    break;
                }
                let _ = tx.send(Message::Ping(Vec::new().into()));
            }

            // 写任务意外结束 → 客户端已断开
            _ = &mut writer => break,
        }
    }

    // —— 清理 ——
    writer.abort();
    on_disconnect(&state, &conn_id, &identity, &game, &tx).await;
    info!(identity = %identity, conn = %conn_id, "WS disconnected");
}

/// 每个客户端消息的处理中心。返回需要直接回给发送者的消息（可选）。
async fn dispatch(
    state: &AppState,
    conn_id: &str,
    identity: &str,
    nick: &str,
    game: &str,
    msg: ClientMessage,
    tx: &mpsc::UnboundedSender<Message>,
) -> Option<ServerMessage> {
    let rid = msg.rid.clone();
    let payload = msg.payload;
    let reply = |t: &str, p: Value| {
        Some(ServerMessage::new(t, p).with_rid(rid.clone()))
    };

    match msg.t.as_str() {
        // 心跳（应用层）
        "ping" => return reply("pong", json!({ "ts": Utc::now().timestamp_millis() })),

        // 请求房间列表
        "room_list" => {
            return reply(
                "room_list",
                json!({ "rooms": state.list_rooms(Some(game), None) }),
            )
        }

        // 请求在线玩家
        "players" => return reply("players", json!({ "players": state.online_players(Some(game)) })),

        // 创建房间（WS 通道也支持，和 REST 等价）
        "room_create" => {
            let name = get_str(&payload, "name").unwrap_or("未命名房间");
            if name.is_empty() || name.len() > 64 {
                return reply("error", json!({ "code": "BAD_NAME", "message": "房间名需 1~64 字符" }));
            }
            let max_players = payload
                .get("max_players")
                .and_then(|v| v.as_u64())
                .unwrap_or(state.config.max_players_default as u64)
                .clamp(2, 64) as usize;
            let visibility = match payload.get("visibility").and_then(|v| v.as_str()) {
                Some("private") => crate::models::RoomVisibility::Private,
                _ => crate::models::RoomVisibility::Public,
            };
            let metadata = payload.get("metadata").cloned().unwrap_or(json!({}));
            let password = payload.get("password").and_then(|v| v.as_str());
            let room = state.create_room(
                game,
                name.to_string(),
                max_players,
                visibility.clone(),
                metadata,
                identity.to_string(),
                nick.to_string(),
                password,
            );
            // 创建者自动入房（房主幂等加入，跳过自己的密码校验）
            let joined = state.join_room(&room.id, &identity.to_string(), nick, password).unwrap_or(room.clone());
            set_conn_room(state, conn_id, Some(joined.id.clone()));
            state.broadcast_room_list(game);
            let _ = tx.send(Message::Text(
                ServerMessage::new("room_joined", json!({ "room": joined })).to_text().into(),
            ));
            return reply("room_created", json!({ "room": joined }));
        }

        // 加入房间
        "room_join" => {
            let room_id = match get_str(&payload, "room") {
                Some(id) => id.to_string(),
                None => {
                    return reply("error", json!({ "code": "MISSING_ROOM", "message": "缺少 room 参数" }))
                }
            };
            let target_game = match state.rooms.get(&room_id) {
                Some(r) => r.value().game.clone(),
                None => {
                    return reply("error", json!({ "code": "NO_ROOM", "message": "房间不存在" }))
                }
            };
            if target_game != game {
                return reply("error", json!({ "code": "GAME_MISMATCH", "message": "房间属于其它游戏" }));
            }

            let password = payload.get("password").and_then(|v| v.as_str());
            // ★ 先校验再加入（含密码/封禁/满员），失败时不影响玩家当前所在房间
            match state.join_room(&room_id, &identity.to_string(), nick, password) {
                Ok(room) => {
                    // 加入成功后才离开上一个房间
                    leave_current_room(state, conn_id, identity, tx).await;
                    set_conn_room(state, conn_id, Some(room.id.clone()));
                    // 通知房内其他人“有人来了”
                    let joined_msg = ServerMessage::new(
                        "peer_joined",
                        json!({
                            "room": room.id,
                            "seat": seat_of(&room, identity),
                        }),
                    )
                    .to_text();
                    state.broadcast_room(&room, &joined_msg, Some(identity));

                    // ★ 广播完整房间状态：房内所有人（包括对局中的房主）据此刷新座位表。
                    //   对局中房主需要拿到新座位，才能把中途加入者纳入权威世界并同步快照。
                    let room_update = ServerMessage::new(
                        "room_update",
                        json!({ "room": room, "reason": "peer_joined" }),
                    )
                    .to_text();
                    state.broadcast_room(&room, &room_update, None);

                    let _ = tx.send(Message::Text(
                        ServerMessage::new("room_joined", json!({ "room": room })).to_text().into(),
                    ));
                    state.broadcast_room_list(game);
                    return reply("joined", json!({ "room": room }));
                }
                Err(e) => {
                    let (code, message) = join_error_parts(&e);
                    return reply("error", json!({ "code": code, "message": message }))
                }
            }
        }

        // 离开当前房间
        "room_leave" => {
            leave_current_room(state, conn_id, identity, tx).await;
            state.broadcast_room_list(game);
            return reply("left", json!({}));
        }

        // ★ 房主踢人：room_kick {target, room?}
        "room_kick" => {
            let room_id = match room_id_for_moderation(state, conn_id, &payload) {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中，且未提供 room 参数" })),
            };
            let target = match get_str(&payload, "target") {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return reply("error", json!({ "code": "MISSING_TARGET", "message": "缺少 target 参数" })),
            };
            match state.kick_player(&room_id, identity, &target) {
                Ok(room) => {
                    // 切断被踢者在房间的收广播通道，并通知其本人
                    let cleared = state.clear_identity_room(&target, &room_id);
                    let notice = ServerMessage::new("kicked", json!({
                        "room": room_id,
                        "by": identity,
                        "reason": get_str(&payload, "reason").unwrap_or("kicked"),
                    })).to_text();
                    state.send_to_identity(&target, &notice);
                    // 通知房内其他人
                    let fwd = ServerMessage::new("peer_kicked", json!({
                        "room": room_id,
                        "target": target,
                        "by": identity,
                        "room_state": room,
                    })).to_text();
                    broadcast_by_room_id(state, &room_id, &fwd);
                    state.broadcast_room_list(game);
                    debug!(by = %identity, target = %target, cleared, "player kicked");
                    return reply("kicked_ok", json!({ "target": target, "room": room_id }));
                }
                Err(e) => {
                    let (code, message) = moderation_error_parts(&e);
                    return reply("error", json!({ "code": code, "message": message }))
                }
            }
        }

        // ★ 房主封禁：room_ban {target, room?}
        "room_ban" => {
            let room_id = match room_id_for_moderation(state, conn_id, &payload) {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中，且未提供 room 参数" })),
            };
            let target = match get_str(&payload, "target") {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return reply("error", json!({ "code": "MISSING_TARGET", "message": "缺少 target 参数" })),
            };
            match state.ban_player(&room_id, identity, &target) {
                Ok(room) => {
                    let cleared = state.clear_identity_room(&target, &room_id);
                    let notice = ServerMessage::new("banned", json!({
                        "room": room_id,
                        "by": identity,
                        "reason": get_str(&payload, "reason").unwrap_or("banned"),
                    })).to_text();
                    state.send_to_identity(&target, &notice);
                    let fwd = ServerMessage::new("peer_banned", json!({
                        "room": room_id,
                        "target": target,
                        "by": identity,
                        "room_state": room,
                    })).to_text();
                    broadcast_by_room_id(state, &room_id, &fwd);
                    state.broadcast_room_list(game);
                    debug!(by = %identity, target = %target, cleared, "player banned");
                    return reply("banned_ok", json!({ "target": target, "room": room_id }));
                }
                Err(e) => {
                    let (code, message) = moderation_error_parts(&e);
                    return reply("error", json!({ "code": code, "message": message }))
                }
            }
        }

        // ★ 房主解封：room_unban {target, room?}
        "room_unban" => {
            let room_id = match room_id_for_moderation(state, conn_id, &payload) {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中，且未提供 room 参数" })),
            };
            let target = match get_str(&payload, "target") {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => return reply("error", json!({ "code": "MISSING_TARGET", "message": "缺少 target 参数" })),
            };
            match state.unban_player(&room_id, identity, &target) {
                Ok(room) => {
                    let fwd = ServerMessage::new("peer_unbanned", json!({
                        "room": room_id,
                        "target": target,
                        "by": identity,
                        "room_state": room,
                    })).to_text();
                    broadcast_by_room_id(state, &room_id, &fwd);
                    // 通知被解封者可以重新加入
                    let notice = ServerMessage::new("unbanned", json!({
                        "room": room_id, "by": identity,
                    })).to_text();
                    state.send_to_identity(&target, &notice);
                    return reply("unbanned_ok", json!({ "target": target, "room": room_id }));
                }
                Err(e) => {
                    let (code, message) = moderation_error_parts(&e);
                    return reply("error", json!({ "code": code, "message": message }))
                }
            }
        }

        // 准备 / 取消准备
        "ready" => {
            let room_id = conn_room(state, conn_id);
            let room_id = match room_id {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中" })),
            };
            let ready = payload.get("value").and_then(|v| v.as_bool()).unwrap_or(true);
            match state.set_ready(&room_id, identity, ready) {
                Ok(room) => {
                    let msg = ServerMessage::new(
                        "room_update",
                        json!({ "room": room, "reason": "ready" }),
                    )
                    .to_text();
                    state.broadcast_room(&room, &msg, None);
                    return None; // 广播里包含自己，不再单独回
                }
                Err(e) => {
                    return reply("error", json!({ "code": "READY_FAILED", "message": e.to_string() }))
                }
            }
        }

        // 房主开始 / 结束对局
        "room_start" | "room_finish" => {
            let room_id = conn_room(state, conn_id);
            let room_id = match room_id {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中" })),
            };
            let room = match state.get_room(&room_id) {
                Ok(r) => r,
                Err(e) => return reply("error", json!({ "code": "NO_ROOM", "message": e.to_string() })),
            };
            if room.host != identity {
                return reply("error", json!({ "code": "NOT_HOST", "message": "仅房主可操作" }));
            }
            let new_state = if msg.t == "room_start" {
                // 新一局：清掉上一局残留的状态快照
                state.clear_snapshot(&room_id);
                RoomState::Playing
            } else {
                RoomState::Finished
            };
            if let Ok(updated) = state.set_room_state(&room_id, new_state.clone()) {
                let msg = ServerMessage::new(
                    "room_state",
                    json!({ "room": updated, "state": new_state }),
                )
                .to_text();
                state.broadcast_room(&updated, &msg, None);
                state.broadcast_room_list(game);
            }
            return None;
        }

        // ★ 游戏自定义事件：原样透传给房间内其他人（服务端不解释 data）
        "game" => {
            let room_id = conn_room(state, conn_id);
            let room_id = match room_id {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中" })),
            };
            let event = get_str(&payload, "event").unwrap_or("").to_string();
            if event.is_empty() {
                return reply("error", json!({ "code": "NO_EVENT", "message": "缺少 event 参数" }));
            }
            let data = payload.get("data").cloned().unwrap_or(json!({}));
            let relayed = RelayedEvent {
                from: identity.to_string(),
                nick: nick.to_string(),
                event,
                data,
                room: room_id.clone(),
                ts: Utc::now(),
            };
            let fwd = ServerMessage::new("game", serde_json::to_value(relayed).unwrap()).to_text();

            // 私发：payload.to = identity
            if let Some(to) = payload.get("to").and_then(|v| v.as_str()) {
                let n = state.send_to_identity(to, &fwd);
                debug!(from = %identity, to = %to, "direct game event, delivered={}", n);
                return reply("sent", json!({ "to": to, "delivered": n }));
            }

            // 否则广播给房间其他人
            if let Some(room) = state.rooms.get(&room_id) {
                state.broadcast_room(room.value(), &fwd, Some(identity));
            }
            return None;
        }

        // 聊天（房间内）
        "chat" => {
            let room_id = conn_room(state, conn_id);
            let text = get_str(&payload, "text").unwrap_or("");
            if text.is_empty() {
                return reply("error", json!({ "code": "EMPTY", "message": "消息不能为空" }));
            }
            let msg = ServerMessage::new(
                "chat",
                json!({
                    "from": identity,
                    "nick": nick,
                    "text": text,
                    "room": room_id,
                    "ts": Utc::now().to_rfc3339(),
                }),
            )
            .to_text();
            if let Some(room_id) = room_id {
                if let Some(room) = state.rooms.get(&room_id) {
                    state.broadcast_room(room.value(), &msg, None);
                }
            } else {
                // 不在房间则发给同 game 的所有人（大厅聊天）
                for conn in state.connections.iter() {
                    if conn.value().game == game {
                        let _ = conn.value().sender.send(Message::Text(msg.clone().into()));
                    }
                }
            }
            return None;
        }

        // ★ 房主提交权威状态快照：网关原样保管，不解释内容。
        // 用途：房主断线时把最后快照通过 state_takeover 交给新房主续跑。
        "state_save" => {
            let room_id = match conn_room(state, conn_id) {
                Some(id) => id,
                None => return reply("error", json!({ "code": "NOT_IN_ROOM", "message": "不在房间中" })),
            };
            let state_value = payload.get("state").cloned().unwrap_or(json!({}));
            // 限制 blob 体积，防止滥用（约 512KB）
            let approx = serde_json::to_string(&state_value).map(|s| s.len()).unwrap_or(0);
            if approx > 512 * 1024 {
                return reply("error", json!({ "code": "SNAPSHOT_TOO_LARGE", "message": "状态快照超过 512KB" }));
            }
            if !state.save_snapshot(&room_id, identity, state_value) {
                return reply("error", json!({ "code": "NOT_HOST", "message": "仅房主可提交状态快照" }));
            }
            return reply("state_saved", json!({ "room": room_id, "bytes": approx }));
        }

        // 强制离开房间（由 REST 解散房间时推过来）
        "force_leave_room" => {
            clear_conn_room(state, conn_id);
            state.broadcast_room_list(game);
            return None;
        }

        _ => {
            return reply(
                "error",
                json!({ "code": "UNKNOWN_TYPE", "message": format!("未知消息类型: {}", msg.t) }),
            )
        }
    }
}

/// 断开连接清理：从所有房间移除席位、更新在线状态、广播通知。
async fn on_disconnect(
    state: &AppState,
    conn_id: &str,
    identity: &str,
    game: &str,
    tx: &mpsc::UnboundedSender<Message>,
) {
    state.connections.remove(conn_id);

    // 只有该身份没有任何连接了，才真正从房间移除席位
    if state.connection_count(identity) == 0 {
        // ★ 在 leave_all_rooms 之前先记录：断线者是否为房主、以及该房间的快照。
        // 因为 leave_all_rooms 内部会移交 host 并可能清空快照，之后就判断不出来了。
        let was_host_rooms: std::collections::HashSet<crate::models::RoomId> = state
            .rooms
            .iter()
            .filter(|r| r.value().host == identity)
            .map(|r| r.value().id.clone())
            .collect();
        let pending_snapshots: std::collections::HashMap<crate::models::RoomId, crate::state::RoomSnapshot> =
            was_host_rooms
                .iter()
                .filter_map(|rid| state.take_snapshot(rid).map(|s| (rid.clone(), s)))
                .collect();

        let affected = state.leave_all_rooms(identity);
        for room_id in affected {
            if let Some(room) = state.rooms.get(&room_id) {
                let mut room = room.value().clone();
                // 标记离线（leave_all_rooms 已移除席位并移交 host，这里 retain 兜底）
                let mut disconnected_seat: Option<Seat> = None;
                room.seats.retain(|s| {
                    if s.identity == identity {
                        disconnected_seat = Some(s.clone());
                        false
                    } else {
                        true
                    }
                });

                if room.seats.is_empty() {
                    drop(room);
                    state.rooms.remove(&room_id);
                    state.clear_snapshot(&room_id);
                } else {
                    let update = ServerMessage::new(
                        "room_update",
                        json!({
                            "room": room,
                            "reason": "peer_left",
                            "seat": disconnected_seat,
                        }),
                    )
                    .to_text();
                    state.broadcast_room(&room, &update, None);

                    // ★ 断线者原为房主且存在状态快照时，把快照交给新房主续跑对局。
                    // 网关不解释 blob 内容，只做保管与转交。
                    if was_host_rooms.contains(&room_id) {
                        if let Some(snap) = pending_snapshots.get(&room_id) {
                            let takeover = ServerMessage::new(
                                "state_takeover",
                                json!({
                                    "room": room_id,
                                    "new_host": room.host,
                                    "from": identity,
                                    "owner": snap.owner,
                                    "state": snap.state,
                                    "updated_at_ms": snap.updated_at_ms,
                                }),
                            )
                            .to_text();
                            let delivered = state.send_to_identity(&room.host, &takeover);
                            state.clear_snapshot(&room_id);
                            info!(
                                room = %room_id, new_host = %room.host,
                                delivered, "state snapshot handed over to new host"
                            );
                        }
                    }
                }
            }
        }
    } else {
        // 该身份还有其它连接：把断开的那个连接所在房间席位标记为离线
        if let Some(rid) = conn_room(state, conn_id) {
            if let Some(mut room) = state.rooms.get_mut(&rid) {
                if let Some(seat) = room.value_mut().seats.iter_mut().find(|s| s.identity == identity) {
                    seat.connected = false;
                }
            }
        }
    }

    let _ = tx.send(Message::Close(None));
    state.broadcast_room_list(game);
    debug!(identity = %identity, "cleanup done");
}

// ——————————————————————————— helpers ———————————————————————————

fn get_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(|v| v.as_str())
}

/// 把加入房间的错误映射为稳定的 WS 错误码，便于客户端区分处理。
fn join_error_parts(e: &AppError) -> (&'static str, String) {
    match e {
        AppError::Unauthorized(_) => ("BAD_PASSWORD", "房间密码错误或未提供".to_string()),
        AppError::ForbiddenBanned(_) => ("BANNED", "你已被该房间封禁，无法加入".to_string()),
        AppError::Conflict(_) => ("JOIN_CONFLICT", e.to_string()),
        AppError::NotFound(_) => ("NO_ROOM", "房间不存在".to_string()),
        _ => ("JOIN_FAILED", e.to_string()),
    }
}

/// 踢人/封禁类操作的错误映射
fn moderation_error_parts(e: &AppError) -> (&'static str, String) {
    match e {
        AppError::Conflict(_) if e.to_string().contains("房主") => ("NOT_HOST", e.to_string()),
        AppError::ForbiddenBanned(_) => ("BANNED", e.to_string()),
        AppError::NotFound(_) => ("NOT_FOUND", e.to_string()),
        AppError::BadRequest(_) => ("BAD_REQUEST", e.to_string()),
        _ => ("MODERATION_FAILED", e.to_string()),
    }
}

fn conn_room(state: &AppState, conn_id: &str) -> Option<String> {
    state.connections.get(conn_id).and_then(|c| c.value().room.clone())
}

/// 管理操作（踢人/封禁/解封）的房间定位：优先用 payload.room，
/// 否则用连接当前所在房间。这样 REST 建房的房主（其 WS 尚未 join）也能指定房间操作。
fn room_id_for_moderation(state: &AppState, conn_id: &str, payload: &Value) -> Option<String> {
    payload
        .get("room")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| conn_room(state, conn_id))
}

fn set_conn_room(state: &AppState, conn_id: &str, room: Option<String>) {
    if let Some(mut conn) = state.connections.get_mut(conn_id) {
        conn.value_mut().room = room;
    }
}

fn clear_conn_room(state: &AppState, conn_id: &str) {
    set_conn_room(state, conn_id, None);
}

fn seat_of(room: &Room, identity: &str) -> Option<Seat> {
    room.seats.iter().find(|s| s.identity == identity).cloned()
}

async fn leave_current_room(
    state: &AppState,
    conn_id: &str,
    identity: &str,
    _tx: &mpsc::UnboundedSender<Message>,
) {
    if let Some(room_id) = conn_room(state, conn_id) {
        clear_conn_room(state, conn_id);
        // 真正移除席位（房主移交 / 空房解散由 state 处理）
        let result = state.leave_room(&room_id, identity);
        let msg = match &result {
            Some((Some(room), _)) => ServerMessage::new(
                "peer_left",
                json!({ "room": room_id, "identity": identity, "room_state": room }),
            )
            .to_text(),
            Some((None, _)) => {
                // 房间已解散
                ServerMessage::new(
                    "room_update",
                    json!({ "room": room_id, "reason": "dissolved" }),
                )
                .to_text()
            }
            None => return,
        };
        // 用更新前的房间做广播范围参考；房间若已解散则从 connections 里按 room 归属发
        broadcast_by_room_id(state, &room_id, &msg);
    }
}

/// 按 room 归属广播（房间对象可能已被删除，所以不依赖 Room）。
fn broadcast_by_room_id(state: &AppState, room_id: &str, text: &str) {
    for conn in state.connections.iter() {
        if conn.value().room.as_deref() == Some(room_id) {
            let _ = conn.value().sender.send(Message::Text(text.to_string().into()));
        }
    }
}

/// 避免 Arc 未使用警告（保留给未来扩展）
#[allow(dead_code)]
fn _arc_marker(_: Arc<u8>) {}
