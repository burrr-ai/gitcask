//! Health/readiness endpoints.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::AppState;

/// Build identity of this binary (commit short sha; `GITCASK_BUILD_SHA` at build time).
/// Exposed by both health endpoints so operators can verify the running artifact.
pub const BUILD_SHA: &str = env!("GITCASK_BUILD_SHA");

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    summary = "Check process health",
    responses((status = 200, description = "Process is healthy", body = crate::web::openapi::HealthResponse)),
    security(())
)]
pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "version": BUILD_SHA}))
}

/// 200 while the instance accepts work; 503 while draining.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "health",
    summary = "Check serving readiness",
    responses(
        (status = 200, description = "Instance accepts work", body = crate::web::openapi::ReadyResponse),
        (status = 503, description = "Instance is draining", body = crate::web::openapi::ReadyResponse)
    ),
    security(())
)]
pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    // Draining after SIGTERM: tell the edge/LB to stop routing here at once
    // (in-flight work finishes; new object work is refused with Retry-After).
    if gitcask_wal::tasks::shutting_down() {
        return (StatusCode::SERVICE_UNAVAILABLE, [(axum::http::header::RETRY_AFTER, "15")], Json(json!({"status": "draining", "version": BUILD_SHA, "running": state.registry.tasks().running_all().len()}))).into_response();
    }
    Json(json!({"status": "ready", "version": BUILD_SHA})).into_response()
}
