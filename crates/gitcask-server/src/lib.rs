//! Git smart HTTP server (protocol v0/v2), LFS, admin, health, metrics.
//! See AGENTS.md Phase 3.

pub mod admin;
pub mod auth;
pub mod bridge;
pub mod cache;
pub mod error;
pub mod events;
pub mod gc;
pub mod health;
pub mod lfs;
pub mod maintain;
pub mod metrics;
pub mod middleware;
pub mod ops;
pub mod repo;
pub mod smart;
pub mod sse;
pub mod static_object;
pub mod stream;
pub mod telemetry;
pub mod web;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::Bytes;
use gitcask_store::DynStore;
use metrics_exporter_prometheus::PrometheusHandle;
use tower::ServiceBuilder;

use crate::error::ApiError;

/// Shared server state.
pub struct AppState {
    pub cfg: Arc<gitcask_config::Config>,
    pub store: DynStore,
    pub registry: Arc<gitcask_wal::Registry>,
    pub auth: Arc<auth::Authenticator>,
    pub semaphores: middleware::RepoSemaphores,
    /// HTTP requests in flight (counted until the response body is done); on the watchdog line.
    pub inflight: Arc<middleware::Inflight>,
    pub caches: cache::ServerCaches,
    pub metrics_handle: Arc<PrometheusHandle>,
    /// The events bridge (`events` role, docs/EVENTS.md): WAL → bus
    /// (`webhook`) from a per-repo cursor. None unless in role with a
    /// bus sink configured.
    pub bridge: Option<Arc<bridge::Bridge>>,
}

impl AppState {
    /// Build a full AppState from a config + store (memory or opened backend).
    pub async fn new(
        cfg: Arc<gitcask_config::Config>,
        store: DynStore,
    ) -> anyhow::Result<Arc<Self>> {
        let registry = gitcask_wal::Registry::new(store.clone(), cfg.clone());
        let bridge = bridge::Bridge::new(&cfg, registry.clone());
        let metrics_handle = metrics::install()?;
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            store,
            registry,
            auth: auth::Authenticator::new(&cfg).await?,
            semaphores: middleware::RepoSemaphores::new(cfg.server.max_concurrent_per_repo),
            inflight: Arc::new(middleware::Inflight::default()),
            caches: cache::ServerCaches::new(&cfg),
            metrics_handle,
            bridge,
        }))
    }
}

/// Build a full axum router.
pub fn router(state: Arc<AppState>) -> Router {
    // Dynamic web responses (JSON API, overview) are compressed on
    // the fly; git smart-HTTP and LFS bytes never are (packs are
    // already compressed, and `Content-Length`/`Range` must stay exact); SSE is
    // excluded by the layer's default predicate.
    let web_compression = tower_http::compression::CompressionLayer::new()
        .br(true)
        .gzip(true)
        .quality(tower_http::CompressionLevel::Fastest);
    // Nothing with content is public: repository handlers perform their own
    // `require_read`/`require_write`/`require_admin`; top-level metrics/docs use
    // `web::require_auth`.
    // `/healthz` and `/readyz` stay open: a startup probe carries no
    // credentials (a 401 there means no revision can ever start — seen 2026-08-20),
    // and they expose only a status word.
    let repository_management = Router::new()
        .merge(
            web::status::router(state.clone())
                .with_state(())
                .layer(web_compression.clone()),
        )
        .merge(
            Router::new()
                .route("/{owner}/{repo}", put(admin::create).delete(admin::delete))
                .layer(web_compression.clone()),
        );
    let repo_routes = Router::new()
        .merge(
            web::api::router(state.clone())
                .with_state(())
                .layer(web_compression.clone()),
        )
        .merge(
            web::v1::router(state.clone())
                .with_state(())
                .layer(web_compression.clone()),
        )
        .merge(repository_management)
        .route(
            "/{owner}/{repo}/info/refs",
            get(smart::info_refs_route)
                .head(repo_method_not_found)
                .fallback(repo_method_not_found),
        )
        .route(
            "/{owner}/{repo}/git-upload-pack",
            post(smart::upload_pack_route).fallback(repo_method_not_found),
        )
        .route(
            "/{owner}/{repo}/git-receive-pack",
            post(smart::receive_pack_route).fallback(repo_method_not_found),
        )
        .route(
            "/{owner}/{repo}/info/lfs/objects/batch",
            post(lfs::batch).fallback(repo_method_not_found),
        )
        .route(
            "/{owner}/{repo}/info/lfs/objects/{oid}",
            get(lfs::get_object)
                .head(lfs::get_object)
                .put(lfs::put_object)
                .fallback(repo_method_not_found),
        )
        .route(
            "/{owner}/{repo}/info/lfs/verify",
            post(lfs::verify).fallback(repo_method_not_found),
        )
        .with_state(state.clone());

    let top_gated = Router::new()
        .route("/metrics", get(metrics::metrics_route))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::require_auth,
        ));
    let docs_gated = Router::new()
        .route("/api/v1/openapi.json", get(web::openapi::openapi_json))
        .route("/api/v1/docs", get(web::openapi::scalar_docs))
        .layer(web_compression.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::require_auth,
        ));
    let inner = Router::new()
        .merge(
            Router::new()
                .route(web::v1::API_V1, get(web::v1::discovery))
                .route(&format!("{}/", web::v1::API_V1), get(web::v1::discovery))
                .layer(web_compression),
        )
        .merge(docs_gated)
        .merge(top_gated)
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        // Events bridge wake-up (docs/EVENTS.md): an S3 bucket notification.
        // The front proxy supplies its principal; 404 when
        // this instance is not a bridge.
        .route(
            "/_events/notify",
            axum::routing::post(
                |axum::extract::State(st): axum::extract::State<Arc<AppState>>,
                 headers: axum::http::HeaderMap,
                 body: Body| async move {
                    bridge::http_notify(&st, &headers, body)
                        .await
                        .unwrap_or_else(|e| e.into_response())
                },
            ),
        )
        .fallback_service(
            ServiceBuilder::new()
                .layer(axum::middleware::map_request(repo::normalize_git_suffix))
                .service(repo_routes),
        )
        // CORS for `server.cors_origins` on `/api*` and `/{o}/{r}/api[-browser]/*`
        // (D27: what the SDK emits; nothing is stripped or rewritten).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::v1::cors,
        ))
        // A panicking handler must only fail its own request (500), never the process.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
        // HTTP/2 carries the host in `:authority`, not `Host`; normalize so every
        // handler can build public URLs from `Host`.
        .layer(axum::middleware::map_request(host_from_authority))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            web::canonical_browser_host,
        ))
        // Outermost: `http.request` span (request_id, trace, principal, repo,
        // status, elapsed) — every log line inside inherits its fields.
        .layer(axum::middleware::from_fn_with_state(
            state.inflight.clone(),
            middleware::request_id,
        ))
        .with_state(state);
    inner
}

async fn repo_method_not_found() -> ApiError {
    ApiError::NotFound("no such route".into())
}

async fn host_from_authority(mut req: Request<Body>) -> Request<Body> {
    if !req.headers().contains_key(axum::http::header::HOST) {
        if let Some(auth) = req.uri().authority().map(|a| a.to_string()) {
            if let Ok(v) = axum::http::HeaderValue::from_str(&auth) {
                req.headers_mut().insert(axum::http::header::HOST, v);
            }
        }
    }
    req
}

fn panic_response(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".to_string());
    tracing::error!(panic = %msg, "request handler panicked");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "internal error",
    )
        .into_response()
}

/// A 1 s ticker that reports when it was not scheduled on time: blocking
/// work on a tokio worker (prod 2026-08-20: minutes-long stalls that also
/// froze the store deadlines) shows up here as "async runtime stalled" with
/// the gap, instead of only as mysterious slow spans everywhere. A late tick
/// is not proof of a blocked worker: the whole process is paused just the
/// same when a serverless host throttles CPU between requests (a service without
/// `--no-cpu-throttling` doing background work) or the memory cgroup reclaims
/// under tmpfs pressure — so the line carries RSS and an explicit caveat
/// (2026-08-22: 11 stalls on a front during a 11.9 GB background prefetch,
/// every one in a gap between requests, none inside one).
fn spawn_runtime_watchdog(
    tasks: Arc<gitcask_wal::tasks::Tasks>,
    inflight: Arc<middleware::Inflight>,
) {
    tokio::spawn(async move {
        let mut last = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let gap = last.elapsed();
            let inflight = inflight.get();
            let tasks_running = tasks.running_count();
            ::metrics::gauge!("gitcask_tasks_running").set(tasks_running as f64);
            if gap > std::time::Duration::from_millis(2500) {
                ::metrics::counter!("gitcask_runtime_stall_total").increment(1);
                ::metrics::histogram!("gitcask_runtime_stall_seconds").record(gap.as_secs_f64());
                let rss_mb = std::fs::read_to_string("/proc/self/statm")
                    .ok()
                    .and_then(|s| {
                        s.split_whitespace()
                            .nth(1)
                            .and_then(|p| p.parse::<u64>().ok())
                    })
                    .map(|pages| pages * 4096 / (1024 * 1024));
                tracing::warn!(
                    gap_ms = gap.as_millis() as u64,
                    inflight,
                    tasks_running,
                    lock_wait_max_ms = gitcask_wal::lockwait::max_wait_ms(),
                    rss_mb,
                    "async runtime stalled (inflight = 0: the process was paused — CPU throttling between requests or memory reclaim; inflight > 0: a worker was blocked or starved, trace it)"
                );
            }
            last = std::time::Instant::now();
        }
    });
}

/// Start periodic local cache eviction for a serving instance.
///
/// The weak registry reference lets test servers that bind axum directly stop
/// the loop by dropping their state, without a second shutdown channel.
pub fn spawn_cache_evictor(state: &Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    if !state.cfg.has_role(gitcask_config::Role::Serve) {
        return None;
    }
    let registry = Arc::downgrade(&state.registry);
    let interval = state.cfg.cache.evict_interval;
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if gitcask_wal::tasks::draining() {
                tracing::debug!("cache eviction skipped while draining");
                continue;
            }
            let Some(registry) = registry.upgrade() else {
                return;
            };
            let result = registry.evict_idle().await;
            let repos = registry.cached_repos().len();
            ::metrics::gauge!("gitcask_cache_repos").set(repos as f64);
            match result {
                Ok(report) => {
                    ::metrics::counter!("gitcask_cache_evicted_total")
                        .increment(report.evicted as u64);
                    tracing::debug!(
                        evicted = report.evicted,
                        remaining_bytes = report.remaining_bytes,
                        repos,
                        "cache eviction pass"
                    );
                    if report.evicted > 0 {
                        tracing::info!(
                            evicted = report.evicted,
                            remaining_bytes = report.remaining_bytes,
                            repos,
                            "cache repositories evicted"
                        );
                    }
                }
                Err(error) => tracing::warn!(%error, "cache eviction pass failed"),
            }
        }
    }))
}

pub(crate) fn request_peer(req: &Request<Body>) -> Option<SocketAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
}

pub(crate) async fn collect_body(body: Body) -> Result<Bytes, ApiError> {
    to_bytes(body, 64 * 1024 * 1024)
        .await
        .map_err(|e| ApiError::BadRequest(format!("body read: {e}")))
}

/// One or two TCP listeners. A loopback bind also takes the other family on the
/// same port (`127.0.0.1` ⇔ `::1`) so `*.localhost` works in browsers, which
/// resolve the name to IPv6 first (a v4-only bind looks like connection refused).
pub(crate) struct TcpAccept {
    listeners: Vec<tokio::net::TcpListener>,
}

impl TcpAccept {
    pub async fn bind(addr: std::net::SocketAddr) -> anyhow::Result<Self> {
        let first = tokio::net::TcpListener::bind(addr).await?;
        let bound = first.local_addr()?;
        let mut listeners = vec![first];
        if let Some(twin) = loopback_twin(bound) {
            match tokio::net::TcpListener::bind(twin).await {
                Ok(l) => listeners.push(l),
                Err(e) => tracing::debug!(%twin, error = %e, "loopback twin not bound"),
            }
        }
        Ok(Self { listeners })
    }

    pub async fn accept(
        &mut self,
    ) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
        match self.listeners.as_mut_slice() {
            [a] => a.accept().await,
            [a, b] => tokio::select! {
                r = a.accept() => r,
                r = b.accept() => r,
            },
            _ => unreachable!("TcpAccept always has 1 or 2 sockets"),
        }
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listeners[0].local_addr()
    }

    pub fn addrs(&self) -> Vec<std::net::SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect()
    }
}

fn loopback_twin(addr: std::net::SocketAddr) -> Option<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match addr.ip() {
        IpAddr::V4(v) if v.is_loopback() => Some((Ipv6Addr::LOCALHOST, addr.port()).into()),
        IpAddr::V6(v) if v.is_loopback() => Some((Ipv4Addr::LOCALHOST, addr.port()).into()),
        _ => None,
    }
}

/// axum `Listener` over [`TcpAccept`] (the dual-stack loopback binder). Nagle
/// is disabled per-connection via `.tap_io(set_nodelay)` at the serve site.
struct NodelayListener(TcpAccept);

impl axum::serve::Listener for NodelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, addr)) => return (stream, addr),
                Err(e) => {
                    tracing::warn!(error = ?e, "TCP accept failed");
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Enable TCP_NODELAY on an accepted stream. Applied via `Listener::tap_io` so
/// the connection stays a plain `TcpStream` and axum's blanket `Connected` impl
/// for `TapIo` supplies the peer `SocketAddr` to `ConnectInfo` (used by the
/// accel-redirect loopback check). Git's receive-pack status is many small
/// pkt-lines; leaving Nagle on turns those into delayed-ACK stalls.
fn set_nodelay(stream: &mut tokio::net::TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!(error = ?e, "failed to set TCP_NODELAY");
    }
}

/// Bind, serve (HTTP/1.1 + h2c), graceful shutdown on `shutdown`.
pub async fn serve(
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr = state.cfg.server.listen;
    let state_for_shutdown = state.clone();
    bridge::spawn_sweeper(state.clone());
    spawn_runtime_watchdog(state.registry.tasks().clone(), state.inflight.clone());
    let listener = TcpAccept::bind(addr).await?;
    let cache_evictor = spawn_cache_evictor(&state);
    let app = router(state);
    tracing::info!(%addr, addrs = ?listener.addrs(), url = %listen_url(&state_for_shutdown.cfg), "gitcask-server listening");

    let st = state_for_shutdown.clone();
    let phase2 = Arc::new(tokio::sync::Notify::new());
    let phase2_tx = phase2.clone();
    let graceful = async move {
        shutdown.await;
        // D31 phase 1 — serving untouched: no new unit starts, the running
        // unit is interrupted at once (D22 redoes it; a unit too expensive to
        // redo is made resumable, not awaited), and we wait for it to be gone.
        gitcask_wal::tasks::begin_drain();
        let interrupted = st.registry.tasks().interrupt_where(crate::ops::is_op);
        tracing::info!(units = ?interrupted, "shutdown signal received: units interrupted, serving continues");
        let deadline = std::time::Instant::now() + UNIT_STOP_MAX;
        loop {
            let running: Vec<_> = st
                .registry
                .tasks()
                .running_all()
                .into_iter()
                .filter(|t| crate::ops::is_op(&t.kind))
                .collect();
            if running.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(count = running.len(), kinds = ?running.iter().map(|t| format!("{}:{}", t.repo, t.kind)).collect::<Vec<_>>(), bound = ?UNIT_STOP_MAX, "shutdown: a unit did not stop within the bound; proceeding");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // D31 phase 2 — serving drain: readyz 503, new object work refused
        // with 503 + Retry-After; in-flight requests get `server.drain_timeout`.
        gitcask_wal::tasks::begin_shutdown();
        phase2_tx.notify_one();
        tracing::info!(drain_timeout = ?st.cfg.server.drain_timeout, "shutdown: serving drain (readyz 503, new object work refused); in-flight requests finish");
        // A beat for the edge/LB to see the 503 before the listener closes.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    };
    let serving = async move {
        use axum::serve::ListenerExt;
        axum::serve(
            NodelayListener(listener).tap_io(set_nodelay),
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(graceful)
        .await
    };
    // In-flight requests get `server.drain_timeout` from phase 2 on (a stuck
    // sideband stream must not hold the restart): axum waits for open
    // connections, we cap that.
    let bound = state_for_shutdown.cfg.server.drain_timeout;
    let result = tokio::select! {
        r = serving => r,
        _ = async { phase2.notified().await; tokio::time::sleep(bound).await } => {
            tracing::warn!(?bound, "shutdown: in-flight requests still open past server.drain_timeout; exiting");
            Ok(())
        }
    };
    if let Some(handle) = cache_evictor {
        handle.abort();
    }
    result?;
    Ok(())
}

/// The origin this process answers at: `server.public_url`, else the HTTP listen address.
/// Loopback is advertised as `gitcask.localhost` (browsers on `localhost` 302 here).
pub fn listen_url(cfg: &gitcask_config::Config) -> String {
    if let Some(u) = &cfg.server.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let ip = cfg.server.listen.ip();
    let host = if ip.is_loopback() || ip.is_unspecified() {
        "gitcask.localhost".to_string()
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    format!("http://{host}:{}", cfg.server.listen.port())
}

/// Phase-1 bound: how long an interrupted unit may take to be gone (its future
/// is dropped on abort; a blocking git child is left to die with the container).
const UNIT_STOP_MAX: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod listen_tests {
    #[tokio::test]
    async fn ipv4_loopback_also_accepts_ipv6() {
        let m = super::TcpAccept::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let port = m.local_addr().unwrap().port();
        if !m.addrs().iter().any(|a| a.is_ipv6()) {
            return; // no IPv6 on this host
        }
        tokio::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port))
            .await
            .expect("::1 twin");
    }
}
