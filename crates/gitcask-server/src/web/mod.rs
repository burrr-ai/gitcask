pub mod api;
pub mod openapi;
pub mod status;
pub mod trailers;
pub mod v1;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::AppState;

/// Send a browser on `localhost` / `127.0.0.1` to `gitcask.localhost` (same port).
/// Git and curl are not browsers and are not redirected.
pub async fn canonical_browser_host(
    State(_st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let skip = path == "/healthz" || path == "/readyz";
    let browser = is_browser(req.headers());
    let get = req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD;
    if get && browser && !skip {
        if let Some(dest) = gitcask_localhost_host(
            req.headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok()),
        ) {
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            let loc = format!("http://{dest}{pq}");
            return (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response();
        }
    }
    next.run(req).await
}

fn is_browser(headers: &axum::http::HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("text/html") {
        return true;
    }
    if headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("document") {
        return true;
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.contains("Mozilla"))
}

fn gitcask_localhost_host(host: Option<&str>) -> Option<String> {
    let host = host?.trim();
    let (name, port) = match host.rsplit_once(':') {
        Some((n, p)) if !n.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        _ => (host, None),
    };
    let name = name.trim_matches(|c| c == '[' || c == ']');
    if !matches!(name, "localhost" | "127.0.0.1" | "::1") {
        return None;
    }
    Some(match port {
        Some(p) => format!("gitcask.localhost:{p}"),
        None => "gitcask.localhost".into(),
    })
}

/// Authentication gate for routes that do not perform their own read/write check.
pub async fn require_auth(
    State(st): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    match st.auth.authenticate(req.headers()).await {
        Ok(_) => next.run(req).await,
        Err(error) => crate::error::ApiError::from(error).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::gitcask_localhost_host;

    #[test]
    fn localhost_becomes_gitcask_localhost_same_port() {
        assert_eq!(
            gitcask_localhost_host(Some("localhost:8080")).as_deref(),
            Some("gitcask.localhost:8080"),
        );
        assert_eq!(
            gitcask_localhost_host(Some("127.0.0.1:8080")).as_deref(),
            Some("gitcask.localhost:8080"),
        );
        assert_eq!(gitcask_localhost_host(Some("gitcask.localhost:8080")), None);
        assert_eq!(gitcask_localhost_host(Some("git.example.com")), None);
    }
}
