//! HTTP surface: the signaling upgrade and a health probe.

use std::sync::Arc;

use axum::{
    Router,
    extract::{State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};

use crate::{config::Config, directory::DirectoryHandle, sfu::WorkerHandle, ws};

/// Everything a request handler needs.
#[derive(Clone)]
pub struct AppState {
    /// Server configuration.
    pub config: Arc<Config>,
    /// Handle to the media worker.
    pub worker: WorkerHandle,
    /// Handle to the directory.
    pub directory: DirectoryHandle,
}

/// Build the HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ws", get(upgrade))
        .with_state(state)
}

/// Upgrade a request to the signaling WebSocket.
async fn upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| ws::serve(socket, state))
}

/// Liveness probe.
async fn health() -> &'static str { "ok" }
