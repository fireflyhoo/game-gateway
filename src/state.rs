//! 全局共享状态：房间表 + 连接池 + 配置。
//! 全部内存态，进程重启即丢失——对局状态应由客户端/业务方持久化。
//! 若需多实例部署，把 RoomRegistry / ConnectionPool 换成 Redis 即可（见 README）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::ws::Message as WsMessage;
use chrono::Utc;
use dashmap::DashMap;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::models::{new_room_id, PlayerId, Room, RoomId, RoomState, RoomVisibility, Seat};

/// 计算房间加入密码的哈希。用 room_id 作为盐，避免相同密码在不同房间产生相同哈希。
pub fn hash_room_password(room_id: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"game-gateway:room-password:v1:");
    hasher.update(room_id.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    // 转 hex
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 每个 WS 连接对应一个出站消息通道；Handler 把消息塞进这里，由写半部发出去。
#[derive(Debug, Clone)]
pub struct Connection {
    pub identity: PlayerId,
    pub nick: String,
    pub game: String,
    pub room: Option<RoomId>,
    pub sender: mpsc::UnboundedSender<WsMessage>,
}

/// 房间权威状态快照：网关原样保存、不解释内容。
/// 用途：房主权威型游戏在房主断线时，把最后快照交给新房主续跑。
#[derive(Debug, Clone)]
pub struct RoomSnapshot {
    /// 不透明状态 blob，由游戏客户端自行定义结构
    pub state: Value,
    /// 写入者 identity（写入时必须是房主）
    pub owner: PlayerId,
    /// 写入时间戳（毫秒）
    pub updated_at_ms: i64,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub rooms: Arc<DashMap<RoomId, Room>>,
    /// key = 连接唯一 id，value = 连接信息
    pub connections: Arc<DashMap<String, Connection>>,
    /// 开发/测试用：预签发的 token 集合（可选，见 auth.rs）
    pub dev_tokens: Arc<DashMap<String, String>>,
    /// key = 房间 id，value = 房主提交的最新状态快照（网关不解释内容）
    pub snapshots: Arc<DashMap<RoomId, RoomSnapshot>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            rooms: Arc::new(DashMap::new()),
            connections: Arc::new(DashMap::new()),
            dev_tokens: Arc::new(DashMap::new()),
            snapshots: Arc::new(DashMap::new()),
        }
    }

    /// 保存房间快照（仅房主可写）。返回是否写入成功。
    pub fn save_snapshot(&self, room_id: &str, owner: &str, state: Value) -> bool {
        let is_host = self
            .rooms
            .get(room_id)
            .map(|r| r.value().host == owner)
            .unwrap_or(false);
        if !is_host {
            return false;
        }
        self.snapshots.insert(
            room_id.to_string(),
            RoomSnapshot {
                state,
                owner: owner.to_string(),
                updated_at_ms: Utc::now().timestamp_millis(),
            },
        );
        true
    }

    /// 取出房间快照的克隆（用于断线移交）
    pub fn take_snapshot(&self, room_id: &str) -> Option<RoomSnapshot> {
        self.snapshots.get(room_id).map(|s| s.value().clone())
    }

    /// 清理房间快照（房间解散 / 对局结束时调用）
    pub fn clear_snapshot(&self, room_id: &str) {
        self.snapshots.remove(room_id);
    }

    pub fn issue_token(&self, token: String, identity: String) {
        self.dev_tokens.insert(identity, token);
    }

    /// 统计某身份当前连接数
    pub fn connection_count(&self, identity: &str) -> usize {
        self.connections
            .iter()
            .filter(|c| c.value().identity == identity)
            .count()
    }

    pub fn list_rooms(&self, game: Option<&str>, visibility: Option<RoomVisibility>) -> Vec<Room> {
        self.rooms
            .iter()
            .filter(|entry| {
                let room = entry.value();
                game.map_or(true, |g| room.game == g)
                    && visibility
                        .as_ref()
                        .map_or(true, |v| &room.visibility == v)
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn get_room(&self, room_id: &str) -> AppResult<Room> {
        self.rooms
            .get(room_id)
            .map(|r| r.value().clone())
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))
    }

    /// 创建房间；创建者自动成为房主并占一个席位（此时尚未连接）。
    /// password 为 Some 且非空串时启用密码保护，仅存储加盐哈希。
    pub fn create_room(
        &self,
        game: &str,
        name: String,
        max_players: usize,
        visibility: RoomVisibility,
        metadata: Value,
        host: PlayerId,
        host_nick: String,
        password: Option<&str>,
    ) -> Room {
        let id = new_room_id();
        let password_hash = password
            .filter(|p| !p.is_empty())
            .map(|p| hash_room_password(&id, p));
        let has_password = password_hash.is_some();
        let room = Room {
            id,
            game: game.to_string(),
            name,
            visibility,
            state: RoomState::Waiting,
            max_players,
            host: host.clone(),
            metadata,
            seats: vec![Seat {
                identity: host,
                nick: host_nick,
                ready: false,
                is_host: true,
                connected: false,
            }],
            created_at: Utc::now(),
            has_password,
            password_hash,
            banned: Vec::new(),
        };
        self.rooms.insert(room.id.clone(), room.clone());
        room
    }

    /// 加入房间，返回加入后的房间快照。
    /// 满员/已结束/游戏中/被封禁/密码错误 都会拒绝。
    /// 已在房间内（幂等重连）跳过密码校验，但仍受封禁检查约束。
    pub fn join_room(
        &self,
        room_id: &str,
        identity: &PlayerId,
        nick: &str,
        password: Option<&str>,
    ) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;

        // 1. 封禁检查优先：被封禁者一律拒绝
        if room.banned.iter().any(|b| b == identity) {
            return Err(AppError::ForbiddenBanned("你已被该房间封禁，无法加入".to_string()));
        }

        // 2. 幂等：已在房间内（重连/房主），跳过状态与密码检查，仅更新在线状态
        if let Some(seat) = room.seats.iter_mut().find(|s| &s.identity == identity) {
            seat.connected = true;
            return Ok(room.clone());
        }

        // 3. 状态检查
        if room.state != RoomState::Waiting {
            return Err(AppError::Conflict("房间已开始或已结束，无法加入".to_string()));
        }

        // 4. 密码检查
        if let Some(expected) = &room.password_hash {
            match password {
                Some(p) if hash_room_password(room_id, p) == *expected => {}
                _ => return Err(AppError::Unauthorized("房间密码错误或未提供".to_string())),
            }
        }

        // 5. 满员检查
        if room.seats.len() >= room.max_players {
            return Err(AppError::Conflict("房间已满".to_string()));
        }
        room.seats.push(Seat {
            identity: identity.clone(),
            nick: nick.to_string(),
            ready: false,
            is_host: false,
            connected: true,
        });
        Ok(room.clone())
    }

    /// 离开房间。房主离开时把房主移交第一个留下的席位；没人则解散房间。
    /// 返回 (更新后的房间 or None, 是否解散)。
    pub fn leave_room(&self, room_id: &str, identity: &str) -> Option<(Option<Room>, bool)> {
        let mut room = self.rooms.get_mut(room_id)?;
        let before = room.seats.len();
        room.seats.retain(|s| s.identity != identity);
        if room.seats.len() == before {
            return None; // 本来就不在房间
        }

        if room.seats.is_empty() {
            drop(room);
            self.rooms.remove(room_id);
            self.clear_snapshot(room_id);
            return Some((None, true));
        }

        if room.host == identity {
            room.host = room.seats[0].identity.clone();
            room.seats[0].is_host = true;
        }
        Some((Some(room.clone()), false))
    }

    /// 房主踢人：把 target 从房间移除。仅房主可用，不能踢自己。
    /// 返回踢人后的房间快照。被踢者仍在封禁列表之外，可重新加入。
    pub fn kick_player(&self, room_id: &str, actor: &str, target: &str) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;
        Self::assert_host(&room, actor)?;
        if target == actor {
            return Err(AppError::BadRequest("不能踢自己".to_string()));
        }
        let before = room.seats.len();
        room.seats.retain(|s| s.identity != target);
        if room.seats.len() == before {
            return Err(AppError::NotFound("该玩家不在房间内".to_string()));
        }
        Ok(room.clone())
    }

    /// 房主封禁：把 target 加入封禁列表并踢出房间。封禁后无法再加入（除非解封）。
    pub fn ban_player(&self, room_id: &str, actor: &str, target: &str) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;
        Self::assert_host(&room, actor)?;
        if target == actor {
            return Err(AppError::BadRequest("不能封禁自己".to_string()));
        }
        if room.banned.iter().any(|b| b == target) {
            return Err(AppError::Conflict("该玩家已被封禁".to_string()));
        }
        room.banned.push(target.to_string());
        room.seats.retain(|s| s.identity != target);
        Ok(room.clone())
    }

    /// 房主解封：把 target 从封禁列表移除。
    pub fn unban_player(&self, room_id: &str, actor: &str, target: &str) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;
        Self::assert_host(&room, actor)?;
        let before = room.banned.len();
        room.banned.retain(|b| b != target);
        if room.banned.len() == before {
            return Err(AppError::NotFound("该玩家不在封禁列表中".to_string()));
        }
        Ok(room.clone())
    }

    /// 断言 actor 是房间房主
    fn assert_host(room: &Room, actor: &str) -> AppResult<()> {
        if room.host != actor {
            return Err(AppError::Conflict("仅房主可执行该操作".to_string()));
        }
        Ok(())
    }

    /// 断开连接时：把该身份从所有房间移除，返回受影响的房间 id 列表。
    pub fn leave_all_rooms(&self, identity: &str) -> Vec<RoomId> {
        let room_ids: Vec<RoomId> = self
            .rooms
            .iter()
            .filter(|r| r.value().seats.iter().any(|s| s.identity == identity))
            .map(|r| r.value().id.clone())
            .collect();

        let mut affected = Vec::new();
        for id in room_ids {
            if self.leave_room(&id, identity).is_some() {
                affected.push(id);
            }
        }
        affected
    }

    /// 设置席位 ready 状态
    pub fn set_ready(&self, room_id: &str, identity: &str, ready: bool) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;
        let seat = room
            .seats
            .iter_mut()
            .find(|s| s.identity == identity)
            .ok_or_else(|| AppError::BadRequest("你不在该房间内".to_string()))?;
        seat.ready = ready;
        Ok(room.clone())
    }

    /// 更新房间状态（start / finish），仅房主可用（权限在调用处校验）
    pub fn set_room_state(&self, room_id: &str, state: RoomState) -> AppResult<Room> {
        let mut room = self
            .rooms
            .get_mut(room_id)
            .ok_or_else(|| AppError::NotFound(format!("房间 {} 不存在", room_id)))?;
        room.state = state;
        Ok(room.clone())
    }

    /// 给房间内所有已连接席位广播文本消息；exclude 用于排除发送者。
    pub fn broadcast_room(
        &self,
        room: &Room,
        text: &str,
        exclude: Option<&str>,
    ) {
        let identities: HashSet<&str> = room
            .seats
            .iter()
            .filter(|s| s.connected)
            .map(|s| s.identity.as_str())
            .collect();
        for conn in self.connections.iter() {
            let c = conn.value();
            // 以“当前连接所在房间”为准，避免给同一身份的其它对局误发
            let in_room = c.room.as_deref() == Some(room.id.as_str())
                || (c.room.is_none() && identities.contains(c.identity.as_str()));
            if in_room && exclude.map_or(true, |e| e != c.identity) {
                if let Err(e) = c.sender.send(WsMessage::Text(text.to_string().into())) {
                    warn!("广播给 {} 失败: {}", c.identity, e);
                }
            }
        }
    }

    /// 广播房间列表变更给同一 game 的所有连接（用于大厅刷新）。
    pub fn broadcast_room_list(&self, game: &str) {
        let rooms = self.list_rooms(Some(game), Some(RoomVisibility::Public));
        let text = serde_json::json!({
            "t": "room_list",
            "payload": { "rooms": rooms }
        })
        .to_string();
        for conn in self.connections.iter() {
            let c = conn.value();
            if c.game == game {
                let _ = c.sender.send(WsMessage::Text(text.clone().into()));
            }
        }
        debug!("broadcast room_list to game={}, rooms={}", game, rooms.len());
    }

    /// 给指定身份的所有连接发消息（多设备登录）
    pub fn send_to_identity(&self, identity: &str, text: &str) -> usize {
        let mut n = 0;
        for conn in self.connections.iter() {
            let c = conn.value();
            if c.identity == identity && c.sender.send(WsMessage::Text(text.to_string().into())).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// 把某身份在所有连接里的 room 归属清掉（用于踢人/封禁时切断其继续收广播）。
    /// 返回被清理的连接数。注意：不主动断开 socket，客户端会收到 kicked/banned 通知自行处理。
    pub fn clear_identity_room(&self, identity: &str, room_id: &str) -> usize {
        let mut n = 0;
        for mut conn in self.connections.iter_mut() {
            let c = conn.value_mut();
            if c.identity == identity && c.room.as_deref() == Some(room_id) {
                c.room = None;
                n += 1;
            }
        }
        n
    }

    /// 在线用户统计（同 game），去重后的 identity -> nick 映射
    pub fn online_players(&self, game: Option<&str>) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for conn in self.connections.iter() {
            let c = conn.value();
            if game.map_or(true, |g| c.game == g) {
                map.entry(c.identity.clone()).or_insert_with(|| c.nick.clone());
            }
        }
        map
    }
}
