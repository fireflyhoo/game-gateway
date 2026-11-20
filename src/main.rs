//! game-gateway —— Web 联机游戏通用后端
//!
//! 提供 REST API + WebSocket 实时通道，服务端不绑定任何具体游戏玩法，
//! 游戏事件由客户端通过 WS 的 "game" 消息原样透传。

mod api;
mod auth;
mod config;
mod docs;
mod error;
mod models;
mod state;
mod ws;

use std::net::SocketAddr;

use anyhow::Result;
use axum::routing::get;
use axum::Router;
use tokio::signal;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    // 日志：RUST_LOG 环境变量控制级别
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();

    let config = Config::from_env();
    let state = AppState::new(config.clone());
    let addr: SocketAddr = config.addr().parse()?;

    // CORS：生产环境建议通过反向代理收敛域名；这里默认放行以便联调
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .merge(docs::routes())          // / , /llms.txt , /ai/*  自描述文档
        .merge(api::routes())           // /health , /api/v1/*
        .route("/ws", get(ws::handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("game-gateway v{} listening on http://{}", env!("CARGO_PKG_VERSION"), addr);
    info!("REST:  /health /api/v1/info /api/v1/rooms /api/v1/auth/dev-token");
    info!("WS:    /ws?token=<jwt>");
    info!("AI文档: /  /llms.txt  /ai/manifest.json  /ai/openapi.json  /ai/capabilities  /ai/guide");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("graceful shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
