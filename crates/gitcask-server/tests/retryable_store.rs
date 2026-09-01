use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use gitcask_store::fault::{FaultPlan, FaultStore};
use tower::ServiceExt;

async fn assert_store_unavailable(
    route: &str,
    response: axum::response::Response,
) -> anyhow::Result<()> {
    let status = response.status();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{route}: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(retry_after.as_ref().expect("retry-after"), "15");
    assert_eq!(
        content_type.as_ref().expect("content-type"),
        "application/json"
    );
    assert_eq!(
        body.as_ref(),
        br#"{"error":"store_unavailable","retryable":true}"#
    );
    Ok(())
}

#[tokio::test]
async fn every_route_maps_retryable_store_errors_to_503() -> anyhow::Result<()> {
    let truth: gitcask_store::DynStore = gitcask_store::memory::MemoryStore::shared();
    let store = FaultStore::new(truth, "retryable-http", 1);
    store.set(FaultPlan {
        p_err_before: 1.0,
        ..Default::default()
    });

    let cache = tempfile::tempdir()?;
    let mut cfg = gitcask_config::Config::default();
    cfg.store.backend = gitcask_config::StoreBackend::Memory;
    cfg.cache.dir = cache.path().to_path_buf();
    let state = gitcask_server::AppState::new(Arc::new(cfg), store).await?;
    let app = gitcask_server::router(state);

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/o/r")
                .body(Body::empty())?,
        )
        .await?;
    assert_store_unavailable("PUT /o/r", create).await?;

    let refs = app
        .oneshot(
            Request::builder()
                .uri("/o/r/api/refs")
                .body(Body::empty())?,
        )
        .await?;
    assert_store_unavailable("GET /o/r/api/refs", refs).await?;
    Ok(())
}

#[tokio::test]
async fn receive_pack_maps_retryable_publish_errors_to_503() -> anyhow::Result<()> {
    let truth: gitcask_store::DynStore = gitcask_store::memory::MemoryStore::shared();
    let store = FaultStore::new(truth, "retryable-publish", 2);
    let cache = tempfile::tempdir()?;
    let mut cfg = gitcask_config::Config::default();
    cfg.store.backend = gitcask_config::StoreBackend::Memory;
    cfg.cache.dir = cache.path().to_path_buf();
    cfg.wal.freshness_ttl = std::time::Duration::from_secs(60);
    cfg.wal.fsck_objects = false;
    cfg.wal.check_connectivity = false;
    let state = gitcask_server::AppState::new(Arc::new(cfg), store.clone()).await?;
    let id = gitcask_git::RepoId::new("o", "push")?;
    let handle = state
        .registry
        .create(&id, gitcask_git::ObjectFormat::Sha1)
        .await?;
    drop(handle.sync_full().await?);

    // The request sync above is still fresh. The first matching operation is
    // therefore the publisher's manifest CAS, whose Retryable error must
    // survive batching and reach the smart-HTTP response as a 503.
    store.set(
        FaultPlan {
            fail_first: 1,
            ..Default::default()
        }
        .with_only(&["manifest.pb"]),
    );
    let zero = "0".repeat(40);
    let new_oid = "1".repeat(40);
    let command = format!("{zero} {new_oid} refs/heads/main\0report-status");
    let mut body = Vec::new();
    gitcask_git::pkt::encode_data(&mut body, command.as_bytes());
    gitcask_git::pkt::encode_flush(&mut body);

    let app = gitcask_server::router(state);
    let push = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/o/push.git/git-receive-pack")
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-receive-pack-request",
                )
                .body(Body::from(body))?,
        )
        .await?;
    assert_store_unavailable("POST /o/push.git/git-receive-pack", push).await?;
    Ok(())
}
