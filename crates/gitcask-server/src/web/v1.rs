//! The non-repo programmatic surface (`/api/v1`) and repository summaries
//! under both lanes of [`crate::web::api::REPO_API_BASES`].
//!
//! Lanes (D27) are a segment *after* the repository prefix:
//! * `/{o}/{r}/api/…` — the primary API lane;
//! * `/{o}/{r}/api-browser/…` — the cross-origin browser lane.
//! Same handlers; lanes differ only by CORS. Non-repo: `/api/v1` discovery.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

use crate::web::api::{Need, RefInfo, etag_for, json_swr, run};
use crate::{AppState, error::ApiError};

/// Canonical prefix of the versioned API.
pub const API_V1: &str = "/api/v1";
pub fn router(state: Arc<AppState>) -> Router {
    let mut r = Router::new();
    // Repo summary/create/delete under both lanes.
    for base in crate::web::api::REPO_API_BASES {
        r = r.route(
            base,
            get(repo_summary)
                .put(crate::admin::create)
                .delete(crate::admin::delete),
        );
    }
    r.with_state(state)
}

// ---- CORS (browser lane from other origins) -----------------------------------

fn origin_allowed(cfg: &gitcask_config::Config, origin: &str) -> bool {
    cfg.server.cors_origins.iter().any(|pat| {
        if let Some((scheme, host)) = pat.split_once("://*.") {
            origin
                .strip_prefix(scheme)
                .and_then(|o| o.strip_prefix("://"))
                .is_some_and(|o| {
                    o.ends_with(host)
                        && o.len() > host.len()
                        && o.as_bytes()[o.len() - host.len() - 1] == b'.'
                        && !o.contains('/')
                })
        } else {
            pat.eq_ignore_ascii_case(origin)
        }
    })
}

const CORS_HEADERS: &str =
    "Authorization, Content-Type, Accept, If-None-Match, If-Range, Range, X-Requested-With";
const CORS_METHODS: &str = "GET, HEAD, POST, PUT, DELETE, OPTIONS";
const CORS_EXPOSE: &str = "ETag, Cache-Control, Content-Type, Content-Length, Content-Range, Accept-Ranges, Content-Disposition, Location";

/// CORS for `/api*`: only origins in `server.cors_origins` get credentials;
/// preflights are answered here (unauthenticated — a preflight carries no
/// credentials by definition); a state-changing request that names an
/// unapproved foreign origin is refused as a cross-origin request guard.
pub async fn cors(State(st): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path();
    let is_api = is_cors_api_path(path);
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Some(origin) = origin.filter(|_| is_api) else {
        return next.run(req).await;
    };
    let allowed = origin_allowed(&st.cfg, &origin);
    if req.method() == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        if allowed {
            cors_headers(r.headers_mut(), &origin);
            r.headers_mut().insert(
                "access-control-allow-methods",
                HeaderValue::from_static(CORS_METHODS),
            );
            r.headers_mut().insert(
                "access-control-allow-headers",
                HeaderValue::from_static(CORS_HEADERS),
            );
            r.headers_mut()
                .insert("access-control-max-age", HeaderValue::from_static("600"));
        }
        return r;
    }
    let same_origin = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| {
            origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                == Some(h)
        });
    if !allowed && !same_origin && !matches!(*req.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::FORBIDDEN,
            "gitcask: cross-origin request from an origin that is not in server.cors_origins\n",
        )
            .into_response();
    }
    let mut resp = next.run(req).await;
    if allowed {
        cors_headers(resp.headers_mut(), &origin);
    }
    resp
}

/// Where CORS applies: the non-repo `/api*` root and the repo lanes
/// `/{o}/{r}/api[-browser](/…)?`. Never git routes.
pub fn is_cors_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || is_repo_api_path(path)
}

/// `/{o}/{r}/api[-browser](/…)?` — the repo-scoped lanes (D27).
pub fn is_repo_api_path(path: &str) -> bool {
    let mut it = path.trim_start_matches('/').splitn(4, '/');
    let (Some(o), Some(r), Some(seg)) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    !o.is_empty() && !r.is_empty() && (seg == "api" || seg == "api-browser")
}

fn cors_headers(h: &mut HeaderMap, origin: &str) {
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert("access-control-allow-origin", v);
    }
    h.insert(
        "access-control-allow-credentials",
        HeaderValue::from_static("true"),
    );
    h.insert(
        "access-control-expose-headers",
        HeaderValue::from_static(CORS_EXPOSE),
    );
    h.append(header::VARY, HeaderValue::from_static("Origin"));
}

// ---- endpoints ---------------------------------------------------------------

#[derive(Serialize)]
struct Discovery<'a> {
    name: &'a str,
    version: u32,
    base: String,
    endpoints: Vec<&'a str>,
}

/// `GET /api/v1` — what this is and where the pieces are.
pub(crate) async fn discovery(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    st.auth.authenticate(&headers).await?;
    let base_url = crate::smart::request_base_url(&st, &headers);
    let body = Discovery {
        name: "gitcask",
        version: 1,
        base: format!("{base_url}{API_V1}"),
        endpoints: vec![
            "GET  /api/v1/openapi.json",
            "GET  /api/v1/docs",
            "-- repository routes live under the repository: /{owner}/{repo}/api/… and /{owner}/{repo}/api-browser/… --",
            "GET|PUT|DELETE /{owner}/{repo}/api",
            "GET  /{owner}/{repo}/api/refs",
            "GET  /{owner}/{repo}/api/refs/{branches|tags}?prefix&q&after&n",
            "PUT|DELETE /{owner}/{repo}/api/refs/heads/{name}",
            "PUT|DELETE /{owner}/{repo}/api/refs/tags/{name}",
            "POST /{owner}/{repo}/api/tags",
            "GET  /{owner}/{repo}/api/archive/{ref}?format&prefix",
            "GET  /{owner}/{repo}/api/resolve/{rev}[/{path}]",
            "GET  /{owner}/{repo}/api/tree/{rev}[/{path}]",
            "GET  /{owner}/{repo}/api/blob/{rev}/{path}[?raw]",
            "GET|POST /{owner}/{repo}/api/commits",
            "POST /{owner}/{repo}/api/merges",
            "GET  /{owner}/{repo}/api/commit/{sha}",
            "GET  /{owner}/{repo}/api/compare/{base}...{head}",
            "GET  /{owner}/{repo}/api/overview",
            "GET  /{owner}/{repo}/api/tasks[/{id}]",
            "GET  /{owner}/{repo}/api/ops",
            "POST /{owner}/{repo}/api/ops/{op}",
        ],
    };
    let mut r = axum::Json(body).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(r)
}

#[derive(Serialize)]
struct RepoSummary {
    owner: String,
    name: String,
    full_name: String,
    head: Option<RefInfo>,
    branches: usize,
    tags: usize,
    clone_url: String,
    html_url: String,
    api_url: String,
}

/// `GET /{owner}/{repo}/api[-browser]` — one cheap, ref-level summary (SWR +
/// ETag on the head sha). Counts are O(1) from the ref index.
async fn repo_summary(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let base_url = crate::smart::request_base_url(&st, &headers);
    let (o, n) = (owner.clone(), name.clone());
    run(
        &st,
        &headers,
        &owner,
        &name,
        Need::Refs,
        None,
        move |r| async move {
            let head = r.index.head().map(|(name, sha)| RefInfo { name, sha });
            let etag = etag_for(head.as_ref().map(|h| h.sha.as_str()).unwrap_or("unborn"));
            let full = format!("{o}/{n}");
            Ok(json_swr(
                &RepoSummary {
                    owner: o,
                    name: n,
                    full_name: full.clone(),
                    head,
                    branches: r.index.branches.len(),
                    tags: r.index.tags.len(),
                    clone_url: format!("{base_url}/{full}.git"),
                    html_url: format!("{base_url}/{full}"),
                    api_url: format!("{base_url}/{full}/api"),
                },
                Some(&etag),
            ))
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_covers_prefix_form_and_v1() {
        assert!(is_cors_api_path("/api/v1"));
        assert!(is_cors_api_path("/acme/monorepo/api"));
        assert!(is_cors_api_path("/acme/monorepo/api/refs"));
        assert!(is_cors_api_path("/acme/monorepo/api-browser/refs"));
        assert!(!is_cors_api_path("/acme/monorepo.git/info/refs"));
    }
}
