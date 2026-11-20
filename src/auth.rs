//! 认证：JWT 签发与校验。
//! 业务方自行做注册登录，用同一个 JWT_SECRET 签发 token 即可接入本服务。
//! 同时提供一个开发用的 /api/v1/auth/dev-token 接口方便联调。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};

use crate::error::{AppError, AppResult};
use crate::models::Claims;
use crate::state::AppState;

/// 从 &str 签发 token
pub fn sign(secret: &str, claims: &Claims) -> AppResult<String> {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("签发 token 失败: {}", e)))
}

/// 校验 token 并反序列化出 Claims
pub fn verify(secret: &str, token: &str) -> AppResult<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 5; // 允许 5 秒时钟偏差
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|d| d.claims)
    .map_err(|_| AppError::Unauthorized("token 无效或已过期".to_string()))
}

/// 从 URL query 中取 token：支持 ?token=xxx
pub fn token_from_query(value: Option<&str>) -> AppResult<String> {
    value
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::Unauthorized("缺少 token 参数".to_string()))
}

/// 从 Authorization: Bearer <token> 中取 token
pub fn token_from_header(value: Option<&str>) -> AppResult<String> {
    let header = value.ok_or_else(|| AppError::Unauthorized("缺少 Authorization 头".to_string()))?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Unauthorized("Authorization 头格式应为 Bearer <token>".to_string()))?;
    Ok(token.to_string())
}

/// Axum 提取器：受保护接口直接写 `auth: AuthUser` 即可拿到 Claims
pub struct AuthUser(pub Claims);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let token = token_from_header(header)?;
        let claims = verify(&state.config.jwt_secret, &token)?;
        Ok(AuthUser(claims))
    }
}

/// 开发一个便捷函数：签发一个 dev token（仅当 JWT_SECRET 为默认值时可用）
pub fn dev_sign(state: &AppState, identity: &str, nick: &str, game: &str) -> AppResult<String> {
    if state.config.jwt_secret.starts_with("dev-only-secret") {
        let claims = Claims::new(identity, nick, game, state.config.jwt_ttl_secs);
        let token = sign(&state.config.jwt_secret, &claims)?;
        state.issue_token(token.clone(), identity.to_string());
        Ok(token)
    } else {
        Err(AppError::ForbiddenDevToken(
            "已配置自定义 JWT_SECRET，dev-token 接口关闭，请由业务方签发 token".to_string(),
        ))
    }
}
