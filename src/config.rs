//! 服务配置：全部通过环境变量注入，方便容器化部署。

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// JWT 签名密钥。生产环境务必通过环境变量覆盖为高强度随机串。
    pub jwt_secret: String,
    /// token 有效期（秒）
    pub jwt_ttl_secs: i64,
    /// 房间默认最大人数
    pub max_players_default: usize,
    /// 单个 JWT 身份允许的最大并发连接数
    pub max_connections_per_identity: usize,
    /// WS 空闲超时（秒）：超过该时间没有任何消息则断开
    pub ws_idle_timeout_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            jwt_secret: env::var("JWT_SECRET")
                .unwrap_or_else(|_| "dev-only-secret-please-change-in-production".to_string()),
            jwt_ttl_secs: env::var("JWT_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(7200),
            max_players_default: env::var("MAX_PLAYERS_DEFAULT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            max_connections_per_identity: env::var("MAX_CONNECTIONS_PER_IDENTITY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            ws_idle_timeout_secs: env::var("WS_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
        }
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
