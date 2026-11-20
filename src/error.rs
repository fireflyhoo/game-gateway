//! 统一错误类型：HTTP 与 WebSocket 共用。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("未认证或凭证无效")]
    Unauthorized(String),

    #[error("资源不存在")]
    NotFound(String),

    #[error("冲突：{0}")]
    Conflict(String),

    #[error("该操作在当前配置下不可用：{0}")]
    ForbiddenDevToken(String),

    #[error("已被封禁：{0}")]
    ForbiddenBanned(String),

    #[error("服务器内部错误")]
    Internal(String),
}

impl AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::ForbiddenDevToken(_) => StatusCode::FORBIDDEN,
            AppError::ForbiddenBanned(_) => StatusCode::FORBIDDEN,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::Unauthorized(_) => "UNAUTHORIZED",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::Conflict(_) => "CONFLICT",
            AppError::ForbiddenDevToken(_) => "DEV_TOKEN_DISABLED",
            AppError::ForbiddenBanned(_) => "BANNED",
            AppError::Internal(_) => "INTERNAL",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = json!({
            "ok": false,
            "error": {
                "code": self.code(),
                "message": self.to_string(),
            }
        });
        (status, Json(body)).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
