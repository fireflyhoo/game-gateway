//! 自描述文档端点：让 AI 客户端连上服务即可自助发现全部能力。
//!
//! 设计：
//! - 静态文档（manifest / openapi / guide / llms.txt）用 include_str! 编译期嵌入，
//!   单二进制部署无外部文件依赖；响应时把 {{BASE_URL}} 替换为实际访问地址。
//! - 动态端点（/ /ai/capabilities）实时反映本实例配置与运行状态。
//! - 所有端点共用 doc_resources() 的资源清单，避免多处声明漂移。

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{json, Value};

use crate::state::AppState;

// 编译期嵌入的文档资产
const MANIFEST_TMPL: &str = include_str!("../docs/ai-manifest.json");
const OPENAPI_TMPL: &str = include_str!("../docs/openapi.json");
const GUIDE: &str = include_str!("../docs/ai-guide.md");
const LLMS_TXT: &str = include_str!("../docs/llms.txt");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(root_discovery))
        .route("/llms.txt", get(llms_txt))
        .route("/ai/manifest.json", get(manifest))
        .route("/ai/openapi.json", get(openapi))
        .route("/ai/capabilities", get(capabilities))
        .route("/ai/guide", get(guide))
}

/// 全部自描述资源的统一清单。info 端点与根文档都复用它，保证一致。
pub fn doc_resources() -> Vec<Value> {
    vec![
        json!({"path": "/", "format": "application/json", "desc": "根发现文档，列出所有自描述资源"}),
        json!({"path": "/api/v1/info", "format": "application/json", "desc": "服务状态与实际限制，含 discovery 指针"}),
        json!({"path": "/llms.txt", "format": "text/plain", "desc": "llmstxt.org 约定的 AI 入口清单"}),
        json!({"path": "/ai/manifest.json", "format": "application/json", "desc": "完整机器可读能力清单"}),
        json!({"path": "/ai/openapi.json", "format": "application/json", "desc": "OpenAPI 3.1 规范（REST 部分）"}),
        json!({"path": "/ai/capabilities", "format": "application/json", "desc": "当前运行实例的实际配置与开关状态"}),
        json!({"path": "/ai/guide", "format": "text/markdown", "desc": "完整接入指南与代码示例"}),
    ]
}

/// 从请求推导对外 base URL：优先信任反向代理注入的 X-Forwarded-*。
fn base_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("http").trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8080")
        .to_string();
    format!("{}://{}", proto, host)
}

fn render_template(tmpl: &str, base: &str) -> String {
    tmpl.replace("{{BASE_URL}}", base)
}

fn text_response(content_type: &'static str, body: String) -> Response {
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    resp
}

// ——————————————————————————— handlers ———————————————————————————

/// 根发现文档：AI 访问根路径即可知道去哪找完整信息。
async fn root_discovery(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    Json(json!({
        "ok": true,
        "service": "game-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Web 联机游戏通用后端，本服务为 AI 客户端提供自描述文档。",
        "base_url": base,
        "start_here": ["/api/v1/info", "/ai/guide", "/ai/manifest.json", "/llms.txt"],
        "resources": doc_resources(),
        "websocket": {
            "endpoint": "/ws?token=<JWT>",
            "hint": "所有 WS 消息结构 {t, payload, rid?}；游戏事件用 t='game' 透传。详见 /ai/guide。"
        }
    }))
}

/// llmstxt.org 约定入口。
async fn llms_txt(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    // llms.txt 内是相对路径，但把 base 作为注释写给 AI，便于其拼接绝对 URL
    let body = format!(
        "# base_url: {}\n\n{}",
        base,
        render_template(LLMS_TXT, &base)
    );
    text_response("text/plain; charset=utf-8", body)
}

/// 完整机器可读清单。
async fn manifest(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    let rendered = render_template(MANIFEST_TMPL, &base);
    match serde_json::from_str::<Value>(&rendered) {
        Ok(v) => Json(v),
        Err(e) => Json(json!({"ok": false, "error": format!("manifest render failed: {}", e)})),
    }
}

/// OpenAPI 3.1 规范。
async fn openapi(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    let rendered = render_template(OPENAPI_TMPL, &base);
    match serde_json::from_str::<Value>(&rendered) {
        Ok(v) => Json(v),
        Err(e) => Json(json!({"openapi": "3.1.0", "error": format!("openapi render failed: {}", e)})),
    }
}

/// 当前实例的实际能力/开关，动态生成。AI 据此判断能否用 dev-token、各项上限等。
async fn capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Value> {
    let base = base_url(&headers);
    let dev_token_enabled = state.config.jwt_secret.starts_with("dev-only-secret");
    let rooms = state.list_rooms(None, None);
    let public_rooms = rooms
        .iter()
        .filter(|r| r.visibility == crate::models::RoomVisibility::Public)
        .count();

    Json(json!({
        "ok": true,
        "service": "game-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "base_url": base,
        "server_time": Utc::now().to_rfc3339(),
        "features": {
            "rest_api": true,
            "websocket": true,
            "room_management": true,
            "game_event_relay": true,
            "private_message": true,
            "multi_game_isolation": true,
            "dev_token_endpoint": dev_token_enabled
        },
        "limits": {
            "max_players_default": state.config.max_players_default,
            "max_players_range": [2, 64],
            "max_connections_per_identity": state.config.max_connections_per_identity,
            "ws_idle_timeout_secs": state.config.ws_idle_timeout_secs,
            "jwt_ttl_secs": state.config.jwt_ttl_secs
        },
        "runtime": {
            "rooms_total": rooms.len(),
            "rooms_public": public_rooms,
            "active_connections": state.connections.len(),
            "online_identities": state.online_players(None).len()
        },
        "auth": {
            "algorithm": "HS256",
            "secret_source": "env JWT_SECRET",
            "production_mode": !dev_token_enabled,
            "note": if dev_token_enabled {
                "当前为开发模式：dev-token 接口可用，请仅用于联调。"
            } else {
                "当前为生产模式：dev-token 接口已关闭，须由业务方签发 JWT。"
            }
        },
        "discovery": { "docs": doc_resources() }
    }))
}

/// 完整接入指南（Markdown）。
async fn guide(headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    let body = render_template(GUIDE, &base);
    text_response("text/markdown; charset=utf-8", body)
}
