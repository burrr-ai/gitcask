//! Events (docs/EVENTS.md): the bridge publishes exactly what the WAL
//! committed, from a durable cursor; the S3-notification wake-up; the sweep;
//! a sink failure keeps the cursor.
mod harness;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

type TestResult = anyhow::Result<()>;
use harness::{Server, TestRepo, git_in};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

type Captured = std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

/// The webhook sink's target: records every event it receives (the bus as
/// the test sees it).
async fn webhook() -> (String, Captured) {
    let captured: Captured = Default::default();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post({
            let captured = captured.clone();
            move |axum::Json(batch): axum::Json<Vec<serde_json::Value>>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().extend(batch);
                    axum::http::StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/events"), captured)
}

async fn switchable_webhook() -> (
    String,
    Captured,
    std::sync::Arc<AtomicBool>,
    std::sync::Arc<AtomicUsize>,
) {
    let captured: Captured = Default::default();
    let online = std::sync::Arc::new(AtomicBool::new(false));
    let attempts = std::sync::Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post({
            let captured = captured.clone();
            let online = online.clone();
            let attempts = attempts.clone();
            move |axum::Json(batch): axum::Json<Vec<serde_json::Value>>| {
                let captured = captured.clone();
                let online = online.clone();
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    if !online.load(Ordering::Acquire) {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE;
                    }
                    captured.lock().unwrap().extend(batch);
                    axum::http::StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/events"), captured, online, attempts)
}

fn bridge_cfg(url: &str, sweep: Duration) -> impl FnOnce(&mut gitcask_config::Config) + '_ {
    move |c| {
        c.events.webhook_url = Some(url.to_string());
        c.events.sweep_interval = sweep;
    }
}

async fn cursor_seq(server: &Server, owner: &str, name: &str) -> Option<u64> {
    use gitcask_store::ObjectStoreExt;
    let id = gitcask_git::RepoId::new(owner, name).unwrap();
    let h = server.state.registry.open(&id).await.unwrap();
    let (_, bytes) = h.store().get_bytes("events/cursor.json").await.unwrap()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["published_seq"].as_u64()
}

async fn wait_for(captured: &Captured, n: usize) -> Vec<serde_json::Value> {
    let t0 = std::time::Instant::now();
    loop {
        let got = captured.lock().unwrap().clone();
        if got.len() >= n {
            return got;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "timed out waiting for {n} events; got {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn s3_notification(object: &str, event_name: &str) -> serde_json::Value {
    serde_json::json!({
        "Records": [{
            "eventName": event_name,
            "s3": { "object": { "key": object } }
        }]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_publishes_from_cursor_exactly_once() -> TestResult {
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::ZERO)).await?;
    let bridge = server.state.bridge.clone().expect("bridge enabled");
    server.put_repo("t", "r").await?;
    let id = gitcask_git::RepoId::new("t", "r")?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    git_in(&src, &["commit", "--allow-empty", "-m", "b"])?;
    git_in(&src, &["push"])?;
    assert!(
        captured.lock().unwrap().is_empty(),
        "nothing reaches the bus until the bridge runs"
    );

    // First catch-up: cold cursor → everything readable (seq 1..=2).
    let r = bridge.catch_up(&id).await?;
    assert_eq!((r.from_seq, r.head_seq, r.emitted, r.gap), (0, 2, 2, None));
    let got = wait_for(&captured, 2).await;
    assert_eq!(got[0]["action"], "create");
    assert_eq!(
        got[0]["old"], ZERO_OID,
        "create carries the zero OID, never empty"
    );
    assert_eq!(got[0]["_gitcask"]["seq"], "1");
    assert_eq!(got[1]["action"], "update");
    assert_eq!(got[1]["_gitcask"]["seq"], "2");
    assert_eq!(got[1]["_gitcask"]["entry_kind"], "push");
    assert_eq!(got[1]["repo"], "t/r");
    assert_eq!(got[1]["ref_name"], "refs/heads/main");
    assert_eq!(got[1]["pusher"], "anon");
    assert!(
        !got[1]["correlation_id"].as_str().unwrap().is_empty(),
        "the request id the middleware minted travels WAL meta → event"
    );
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(2));

    // Again: nothing new, nothing published, cursor untouched.
    let r = bridge.catch_up(&id).await?;
    assert_eq!((r.from_seq, r.head_seq, r.emitted), (2, 2, 0));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(captured.lock().unwrap().len(), 2);

    // The S3 notification of the manifest CAS is the wake-up.
    git_in(&src, &["push", "origin", ":refs/heads/main"])?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/_events/notify", server.base_url))
        .json(&s3_notification(
            "repos/t/r/manifest.pb",
            "ObjectCreated:Put",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let report: serde_json::Value = resp.json().await?;
    assert_eq!(report[0]["emitted"], 1);
    let got = wait_for(&captured, 3).await;
    assert_eq!(got[2]["action"], "delete");
    assert_eq!(
        got[2]["new"], ZERO_OID,
        "delete carries the zero OID, never empty"
    );
    assert_eq!(cursor_seq(&server, "t", "r").await, Some(3));

    // Another S3-shaped notification and the plain shapes wake the same catch-up;
    // with nothing new they are acked with an empty report list.
    for body in [
        serde_json::json!({"Records": [{"eventName": "ObjectCreated:Put", "s3": {"object": {"key": "repos/t/r/manifest.pb"}}}]}),
        serde_json::json!({"repo": "t/r"}),
        serde_json::json!({"key": "repos/t/r/manifest.pb"}),
    ] {
        let resp = client
            .post(format!("{}/_events/notify", server.base_url))
            .json(&body)
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "{body}");
        let report: serde_json::Value = resp.json().await?;
        assert_eq!(report[0]["emitted"], 0, "{body}: {report}");
    }

    // Other objects and other event types are acked and ignored.
    for (obj, event_name) in [
        ("repos/t/r/wal/abc.pack", "ObjectCreated:Put"),
        ("repos/t/r/manifest.pb", "ObjectRemoved:Delete"),
        ("repos/t/r/events/cursor.json", "ObjectCreated:Put"),
    ] {
        let resp = client
            .post(format!("{}/_events/notify", server.base_url))
            .json(&s3_notification(obj, event_name))
            .send()
            .await?;
        assert_eq!(resp.status(), 200, "{obj} {event_name}");
    }
    // A late notification for a repo deleted since: 200, nothing to do
    // (a 503 would make the notifier retry it for days).
    let del = client
        .delete(format!("{}/t/r", server.base_url))
        .send()
        .await?;
    assert_eq!(del.status(), 204);
    let resp = client
        .post(format!("{}/_events/notify", server.base_url))
        .json(&s3_notification(
            "repos/t/r/manifest.pb",
            "ObjectCreated:Put",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(captured.lock().unwrap().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_sweep_timer_publishes_without_notifications() -> TestResult {
    let (url, captured) = webhook().await;
    let server = Server::start_with_tweak(bridge_cfg(&url, Duration::from_millis(200))).await?;
    server.put_repo("t", "r").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    let got = wait_for(&captured, 1).await;
    assert_eq!(got[0]["action"], "create");
    assert_eq!(got[0]["_gitcask"]["seq"], "1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_sweep_finds_pending_repo_after_cache_eviction() -> TestResult {
    let (url, captured, online, attempts) = switchable_webhook().await;
    let server = Server::start_with_tweak(|c| {
        bridge_cfg(&url, Duration::ZERO)(c);
        c.cache.evict_idle_after = Duration::ZERO;
        c.cache.evict_interval = Duration::from_secs(3600);
    })
    .await?;
    let bridge = server.state.bridge.clone().expect("bridge enabled");
    server.put_repo("t", "evicted").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "evicted")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;

    let report = server.state.registry.evict_idle().await?;
    assert_eq!(report.evicted, 1);
    assert!(server.state.registry.cached_repos().is_empty());

    bridge.sweep().await;
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert!(captured.lock().unwrap().is_empty());
    assert_eq!(cursor_seq(&server, "t", "evicted").await, None);

    online.store(true, Ordering::Release);
    bridge.sweep().await;
    let got = wait_for(&captured, 1).await;
    assert_eq!(got[0]["_gitcask"]["seq"], "1");
    assert_eq!(cursor_seq(&server, "t", "evicted").await, Some(1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_sink_failure_keeps_the_cursor() -> TestResult {
    // Nothing listens here: every delivery fails.
    let server =
        Server::start_with_tweak(bridge_cfg("http://127.0.0.1:1/events", Duration::ZERO)).await?;
    let bridge = server.state.bridge.clone().expect("bridge enabled");
    server.put_repo("t", "r").await?;
    let id = gitcask_git::RepoId::new("t", "r")?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "-u", "origin", "main"])?;

    let err = bridge.catch_up(&id).await.expect_err("sink down");
    assert!(err.to_string().contains("webhook sink"), "{err:#}");
    assert_eq!(
        cursor_seq(&server, "t", "r").await,
        None,
        "cursor must not advance"
    );

    let resp = reqwest::Client::new()
        .post(format!("{}/_events/notify", server.base_url))
        .json(&s3_notification(
            "repos/t/r/manifest.pb",
            "ObjectCreated:Put",
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 503, "non-2xx so the notifier redelivers");
    Ok(())
}
