//! Initialization is an ordinary, independently durable Git repository write.

mod harness;

use std::{process::Command, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use gitcask_git::RepoId;
use gitcask_server::{AppState, router};
use gitcask_store::fault::{FaultPlan, FaultStore};
use harness::{Server, git_in};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Source {
    work: tempfile::TempDir,
    pinned: String,
    tree: String,
    old: String,
    secret: String,
}

async fn source(server: &Server, format: &str) -> Result<Source> {
    let client = reqwest::Client::new();
    ensure!(
        client
            .put(format!(
                "{}/seed/source?object_format={format}",
                server.base_url
            ))
            .send()
            .await?
            .status()
            == 201
    );
    let work = tempfile::tempdir()?;
    git_in(
        work.path(),
        &[
            "init",
            "-q",
            "-b",
            "main",
            &format!("--object-format={format}"),
        ],
    )?;
    std::fs::write(
        work.path().join("removed-secret"),
        "historical sentinel, never copy this blob\n",
    )?;
    git_in(work.path(), &["add", "."])?;
    git_in(work.path(), &["commit", "-qm", "old history"])?;
    let old = git_in(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let secret = git_in(work.path(), &["rev-parse", "HEAD:removed-secret"])?
        .trim()
        .to_string();
    git_in(work.path(), &["rm", "-q", "removed-secret"])?;
    std::fs::write(work.path().join("binary"), [0, 255, 1, 128, 0])?;
    std::fs::write(
        work.path().join("run.sh"),
        "#!/bin/sh\nprintf initialized\\n\n",
    )?;
    std::fs::write(
        work.path().join(".gitmodules"),
        "[submodule \"module\"]\n\tpath = module\n\turl = https://example.invalid/external.git\n",
    )?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("binary", work.path().join("link"))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "version https://git-lfs.github.com/spec/v1",
        work.path().join("pointer-like-link"),
    )?;
    git_in(work.path(), &["add", "."])?;
    git_in(work.path(), &["update-index", "--chmod=+x", "run.sh"])?;
    git_in(
        work.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{old},module"),
        ],
    )?;
    git_in(work.path(), &["commit", "-qm", "pinned tree"])?;
    let pinned = git_in(work.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let tree = git_in(work.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    git_in(
        work.path(),
        &["push", "-q", &server.repo_url("seed", "source"), "main"],
    )?;
    Ok(Source {
        work,
        pinned,
        tree,
        old,
        secret,
    })
}

fn body(pinned: &str) -> Value {
    json!({
        "source": {"owner":"seed", "repo":"source", "commit_oid":pinned},
        "branch":"start", "operation_key":"creation-1", "message":"Independent root\n",
        "committer":{"name":"Builder", "email":"builder@example.test", "when":"2026-09-28T03:00:00+09:00"}
    })
}

async fn initialize(server: &Server, target: &str, body: &Value) -> Result<(u16, Value)> {
    let response = reqwest::Client::new()
        .post(format!("{}/seed/{target}/api/initialize", server.base_url))
        .header("Accept", "application/json")
        .json(body)
        .send()
        .await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    ))
}

async fn create(server: &Server, name: &str, format: &str) -> Result<()> {
    ensure!(
        reqwest::Client::new()
            .put(format!(
                "{}/seed/{name}?object_format={format}",
                server.base_url
            ))
            .send()
            .await?
            .status()
            == 201
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_root_exact_tree_cold_clone_and_normal_git_operations() -> Result<()> {
    for format in ["sha1", "sha256"] {
        let server = Server::start().await?;
        let src = source(&server, format).await?;
        create(&server, "destination", format).await?;
        let request = body(&src.pinned);
        // Moving the source branch after pinning must not change initialization.
        std::fs::write(src.work.path().join("later"), "not in pinned tree")?;
        git_in(src.work.path(), &["add", "."])?;
        git_in(src.work.path(), &["commit", "-qm", "branch moved"])?;
        git_in(
            src.work.path(),
            &["push", "-q", &server.repo_url("seed", "source"), "main"],
        )?;
        // Replacement refs cannot change the meaning of a pinned object ID.
        git_in(
            src.work.path(),
            &[
                "push",
                "-q",
                &server.repo_url("seed", "source"),
                &format!("HEAD:refs/replace/{}", src.pinned),
            ],
        )?;
        let (status, result) = initialize(&server, "destination", &request).await?;
        assert_eq!(status, 201, "{result}");
        assert_eq!(result["tree_oid"], src.tree);
        assert_eq!(result["ref"], "refs/heads/start");
        let root = result["commit_oid"].as_str().context("root oid")?;
        assert_ne!(root, src.pinned);
        // Delete all source bucket bytes, then use an instance with no warm cache.
        let source_id = RepoId::new("seed", "source")?;
        server.state.registry.delete(&source_id).await?;
        let cold = server.start_sibling_with(|_| {}).await?;
        let clone = tempfile::tempdir()?;
        git_in(
            clone.path(),
            &["clone", "-q", &cold.repo_url("seed", "destination"), "."],
        )?;
        assert_eq!(
            git_in(clone.path(), &["rev-parse", "HEAD^{tree}"])?.trim(),
            src.tree
        );
        assert_eq!(
            git_in(clone.path(), &["rev-list", "--all", "--count"])?.trim(),
            "1"
        );
        assert_eq!(
            git_in(clone.path(), &["rev-list", "--parents", "-n1", "HEAD"])?.trim(),
            root
        );
        assert_eq!(
            std::fs::read(clone.path().join("binary"))?,
            [0, 255, 1, 128, 0]
        );
        assert!(git_in(clone.path(), &["ls-tree", "HEAD", "run.sh"])?.starts_with("100755"));
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(clone.path().join("link"))?,
            std::path::PathBuf::from("binary")
        );
        assert!(git_in(clone.path(), &["ls-tree", "HEAD", "module"])?.starts_with("160000"));
        assert!(!clone.path().join("later").exists());
        let handle = cold
            .state
            .registry
            .open(&RepoId::new("seed", "destination")?)
            .await?;
        let guard = handle.sync_full().await?;
        assert!(
            !handle
                .local()
                .path()
                .join("objects/info/alternates")
                .exists()
        );
        for oid in [&src.old, &src.pinned, &src.secret] {
            assert!(
                !Command::new("git")
                    .current_dir(handle.local().path())
                    .args(["cat-file", "-e", oid])
                    .output()?
                    .status
                    .success()
            );
            assert!(
                !Command::new("git")
                    .current_dir(clone.path())
                    .args(["fetch", "-q", "origin", oid])
                    .output()?
                    .status
                    .success(),
                "historical object {oid} was fetchable"
            );
        }
        drop(guard);
        git_in(clone.path(), &["fsck", "--full", "--no-dangling"])?;
        std::fs::write(
            clone.path().join("after"),
            "ordinary push after initialization",
        )?;
        git_in(clone.path(), &["add", "."])?;
        git_in(clone.path(), &["commit", "-qm", "normal write"])?;
        git_in(clone.path(), &["push", "-q", "origin", "start"])?;
        git_in(clone.path(), &["tag", "v1"])?;
        git_in(
            clone.path(),
            &["push", "-q", "origin", "v1", "HEAD:refs/heads/feature"],
        )?;
        handle.write_checkpoint().await?;
        let newer = git_in(clone.path(), &["rev-parse", "HEAD"])?;
        let cold_again = cold.start_sibling_with(|_| {}).await?;
        let (status, replay) = initialize(&cold_again, "destination", &request).await?;
        assert_eq!(status, 200, "{replay}");
        assert_eq!(replay["commit_oid"], root);
        assert_eq!(replay["seq"], result["seq"]);
        assert_eq!(replay["replayed"], true);
        assert!(
            cold_again
                .ls_remote("seed", "destination")
                .await?
                .contains(newer.trim())
        );
        let mut changed = request.clone();
        changed["message"] = json!("different payload");
        assert_eq!(
            initialize(&cold_again, "destination", &changed).await?.0,
            409
        );
        changed = request.clone();
        changed["operation_key"] = json!("different-key");
        assert_eq!(
            initialize(&cold_again, "destination", &changed).await?.0,
            409
        );
        changed = request.clone();
        changed["author"] = changed["committer"].clone();
        changed["source"]["commit_oid"] = json!(src.pinned.to_ascii_uppercase());
        assert_eq!(
            initialize(&cold_again, "destination", &changed).await?.0,
            200
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_nonpristine_and_invalid_inputs_without_publication() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    create(&server, "destination", "sha1").await?;
    let request = body(&src.pinned);
    for invalid in [
        "main".to_string(),
        src.pinned.chars().take(12).collect(),
        "0".repeat(40),
        "../main".into(),
    ] {
        let mut bad = request.clone();
        bad["source"]["commit_oid"] = json!(invalid);
        assert_eq!(initialize(&server, "destination", &bad).await?.0, 400);
    }
    let mut bad = request.clone();
    bad["source"]["commit_oid"] = json!(src.tree);
    assert_eq!(initialize(&server, "destination", &bad).await?.0, 400);
    bad["source"]["commit_oid"] = json!("a".repeat(40));
    assert_eq!(initialize(&server, "destination", &bad).await?.0, 404);
    bad = request.clone();
    bad["unexpected"] = json!(true);
    assert_eq!(initialize(&server, "destination", &bad).await?.0, 400);
    bad = request.clone();
    bad["branch"] = json!("../outside");
    assert_eq!(initialize(&server, "destination", &bad).await?.0, 400);
    bad = request.clone();
    bad["committer"]["when"] = json!("yesterday");
    assert_eq!(initialize(&server, "destination", &bad).await?.0, 400);
    create(&server, "sha256", "sha256").await?;
    assert_eq!(initialize(&server, "sha256", &request).await?.0, 400);
    let handle = server
        .state
        .registry
        .open(&RepoId::new("seed", "destination")?)
        .await?;
    assert_eq!(handle.manifest().head_seq, 0);
    assert!(handle.manifest().initialization.is_none());
    git_in(
        src.work.path(),
        &[
            "push",
            "-q",
            &server.repo_url("seed", "destination"),
            "main:other",
        ],
    )?;
    assert_eq!(initialize(&server, "destination", &request).await?.0, 409);
    git_in(
        src.work.path(),
        &[
            "push",
            "-q",
            &server.repo_url("seed", "destination"),
            ":other",
        ],
    )?;
    assert_eq!(initialize(&server, "destination", &request).await?.0, 409);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lfs_pointers_fail_closed_and_large_ordinary_blobs_remain_supported() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    create(&server, "destination", "sha1").await?;
    for version in [
        "https://git-lfs.github.com/spec/v1",
        "https://hawser.github.com/spec/v1",
    ] {
        std::fs::write(
            src.work.path().join("pointer"),
            format!(
                "version {version}\noid sha256:{}\nsize 2000\n",
                "a".repeat(64)
            ),
        )?;
        git_in(src.work.path(), &["add", "."])?;
        git_in(src.work.path(), &["commit", "-qm", "pointer"])?;
        git_in(
            src.work.path(),
            &["push", "-q", &server.repo_url("seed", "source"), "main"],
        )?;
        let pinned = git_in(src.work.path(), &["rev-parse", "HEAD"])?;
        let (status, response) = initialize(&server, "destination", &body(pinned.trim())).await?;
        assert_eq!(status, 422, "{response}");
    }
    // >1024 bytes is not an LFS pointer; only its tree metadata is inspected.
    std::fs::write(
        src.work.path().join("pointer"),
        format!(
            "version https://git-lfs.github.com/spec/v1\n{}",
            "x".repeat(2 * 1024 * 1024)
        ),
    )?;
    git_in(src.work.path(), &["add", "."])?;
    git_in(src.work.path(), &["commit", "-qm", "ordinary large blob"])?;
    git_in(
        src.work.path(),
        &["push", "-q", &server.repo_url("seed", "source"), "main"],
    )?;
    let pinned = git_in(src.work.path(), &["rev-parse", "HEAD"])?;
    let result = initialize(&server, "destination", &body(pinned.trim())).await?;
    assert_eq!(result.0, 201, "{}", result.1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requires_source_read_and_destination_write_even_for_replays() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    create(&server, "destination", "sha1").await?;
    let (private, public) = gitcask_server::auth::generate_key_pair_pem()?;
    let secure = server
        .start_sibling_with(|cfg| {
            cfg.server.auth_mode = gitcask_config::AuthMode::Jwt;
            cfg.auth.jwt.public_key = Some(public);
            cfg.auth.jwt.issuer = "initialize-test".into();
        })
        .await?;
    let request = body(&src.pinned);
    let client = reqwest::Client::new();
    let url = format!("{}/seed/destination/api/initialize", secure.base_url);
    assert_eq!(client.post(&url).json(&request).send().await?.status(), 401);
    for scopes in [
        vec!["seed/destination:write"],
        vec!["seed/source:read"],
        vec!["seed/source:read", "seed/destination:read"],
    ] {
        let token = gitcask_server::auth::mint_token(
            &private,
            "initialize-test",
            None,
            "tester",
            &scopes.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            Duration::from_mins(1),
        )?;
        assert_eq!(
            client
                .post(&url)
                .bearer_auth(token)
                .json(&request)
                .send()
                .await?
                .status(),
            404
        );
    }
    let token = gitcask_server::auth::mint_token(
        &private,
        "initialize-test",
        None,
        "tester",
        &["seed/source:read".into(), "seed/destination:write".into()],
        Duration::from_mins(1),
    )?;
    let response = client
        .post(&url)
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await?;
    assert_eq!(response.status(), 201, "{}", response.text().await?);
    server
        .state
        .registry
        .delete(&RepoId::new("seed", "source")?)
        .await?;
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(&token)
            .json(&request)
            .send()
            .await?
            .status(),
        200
    );
    let no_source = gitcask_server::auth::mint_token(
        &private,
        "initialize-test",
        None,
        "tester",
        &["seed/destination:write".into()],
        Duration::from_mins(1),
    )?;
    assert_eq!(
        client
            .post(&url)
            .bearer_auth(&no_source)
            .json(&request)
            .send()
            .await?
            .status(),
        404
    );
    Ok(())
}

async fn fault_state(
    server: &Server,
) -> Result<(Arc<AppState>, Arc<FaultStore>, tempfile::TempDir)> {
    let cache = tempfile::tempdir()?;
    let mut config = (*server.state.cfg).clone();
    config.cache.dir = cache.path().to_path_buf();
    config.git.commit_graph = false;
    config.wal.snapshot_every_entries = 0;
    let link = FaultStore::new(server.store.clone(), "initialize", 42);
    link.set_trace(true);
    let state = AppState::new(Arc::new(config), link.clone()).await?;
    Ok((state, link, cache))
}

async fn call(state: Arc<AppState>, target: &str, request: &Value) -> Result<(u16, Value)> {
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/seed/{target}/api/initialize"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(request)?))?,
        )
        .await?;
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_failures_ambiguous_success_and_request_budgets() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    let request = body(&src.pinned);
    for target in ["before", "ambiguous", "budget"] {
        create(&server, target, "sha1").await?;
    }
    let (state, link, _cache) = fault_state(&server).await?;
    let source = state.registry.open(&RepoId::new("seed", "source")?).await?;
    drop(source.sync_full().await?);
    let destination = state.registry.open(&RepoId::new("seed", "budget")?).await?;
    link.take_trace();
    let before = link.stats().ops.load(std::sync::atomic::Ordering::Relaxed);
    let result = call(state.clone(), "budget", &request).await?;
    assert_eq!(result.0, 201, "{}", result.1);
    let used = link.stats().ops.load(std::sync::atomic::Ordering::Relaxed) - before;
    assert_eq!(used, 7, "warm initialize: {used} requests");
    assert!(link.take_trace().iter().all(|line| !line.contains("list")));
    let before = link.stats().ops.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(call(state.clone(), "budget", &request).await?.0, 200);
    assert_eq!(
        link.stats().ops.load(std::sync::atomic::Ordering::Relaxed) - before,
        1
    );
    assert_eq!(destination.manifest().head_seq, 1);
    link.set(FaultPlan {
        fail_once_keys: vec!["put:repos/seed/before/manifest.pb".into()],
        ..Default::default()
    });
    assert_eq!(call(state.clone(), "before", &request).await?.0, 503);
    link.heal();
    let untouched = server
        .state
        .registry
        .open(&RepoId::new("seed", "before")?)
        .await?;
    drop(untouched.sync_refs().await?);
    assert_eq!(untouched.manifest().head_seq, 0);
    assert!(untouched.manifest().initialization.is_none());
    let retried = call(state.clone(), "before", &request).await?;
    assert_eq!(retried.0, 201, "{}", retried.1);
    link.set(FaultPlan {
        p_err_after: 1.0,
        only_keys: Some(vec!["repos/seed/ambiguous/manifest.pb".into()]),
        ..Default::default()
    });
    let ambiguous = call(state.clone(), "ambiguous", &request).await?;
    assert_eq!(ambiguous.0, 201, "{}", ambiguous.1);
    assert!(
        link.stats()
            .err_after
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    link.heal();
    // Simulate discarding the successful HTTP response and retrying on a cold instance.
    let cold = server.start_sibling_with(|_| {}).await?;
    let replay = initialize(&cold, "ambiguous", &request).await?;
    assert_eq!(replay.0, 200, "{}", replay.1);
    assert_eq!(replay.1["commit_oid"], ambiguous.1["commit_oid"]);
    assert_eq!(replay.1["seq"], ambiguous.1["seq"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_initializers_race_across_instances_and_exact_retries_join() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    let sibling = server.start_sibling_with(|_| {}).await?;
    for same_request in [false, true] {
        let target = if same_request { "same" } else { "different" };
        create(&server, target, "sha1").await?;
        let one = body(&src.pinned);
        let mut two = one.clone();
        if !same_request {
            two["operation_key"] = json!("competing");
            two["branch"] = json!("different-branch");
        }
        let (a, b) = tokio::join!(
            initialize(&server, target, &one),
            initialize(&sibling, target, &two)
        );
        let mut statuses = [a?.0, b?.0];
        statuses.sort_unstable();
        assert_eq!(statuses, if same_request { [200, 201] } else { [201, 409] });
        let handle = server
            .state
            .registry
            .open(&RepoId::new("seed", target)?)
            .await?;
        drop(handle.sync_refs().await?);
        assert_eq!(handle.read_log(1, None).await?.len(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_narrates_initialization_and_pack_limit_leaves_target_pristine() -> Result<()> {
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    create(&server, "destination", "sha1").await?;
    let request = body(&src.pinned);
    let limited = server
        .start_sibling_with(|cfg| cfg.server.max_push_bytes = bytesize::ByteSize::b(16))
        .await?;
    assert_eq!(initialize(&limited, "destination", &request).await?.0, 413);
    let response = reqwest::Client::new()
        .post(format!(
            "{}/seed/destination/api/initialize",
            server.base_url
        ))
        .header("Accept", "text/event-stream")
        .json(&request)
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()?
            .starts_with("text/event-stream")
    );
    let stream = response.text().await?;
    assert!(stream.contains("event: notice"), "{stream}");
    assert!(stream.contains("event: result"), "{stream}");
    assert!(!stream.contains("event: error"), "{stream}");
    Ok(())
}

// The forwarding secret is process configuration; isolate this scenario rather
// than mutating the parallel test process's environment.
#[test]
fn combined_management_and_scoped_introspection() -> Result<()> {
    if std::env::var_os("GITCASK_INITIALIZE_AUTH_CHILD").is_none() {
        let status = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "combined_management_and_scoped_introspection",
                "--nocapture",
            ])
            .env("GITCASK_INITIALIZE_AUTH_CHILD", "1")
            .env("GITCASK_FORWARD_SECRET", "initialize-proxy")
            .env("GITCASK_INITIALIZE_INTROSPECT_SECRET", "issuer-service")
            .status()?;
        ensure!(
            status.success(),
            "combined initialization auth child failed"
        );
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(combined_auth())
}

async fn combined_auth() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let issuer = format!("http://{}", listener.local_addr()?);
    let introspector = axum::Router::new().route(
        "/",
        axum::routing::post(|axum::Json(request): axum::Json<Value>| async move {
            let scopes = match request.get("token").and_then(Value::as_str) {
                Some("both") => vec!["seed/source:read", "seed/destination:write"],
                Some("target-only") => vec!["seed/destination:write"],
                Some("source-only") => vec!["seed/source:read"],
                _ => Vec::new(),
            };
            axum::Json(json!({"active":true, "principal":"introspected", "scopes":scopes, "ttl":0}))
        }),
    );
    let issuer_task = tokio::spawn(async move { axum::serve(listener, introspector).await });
    let server = Server::start().await?;
    let src = source(&server, "sha1").await?;
    for target in ["destination", "forwarded"] {
        create(&server, target, "sha1").await?;
    }
    let secure = server
        .start_sibling_with(|cfg| {
            cfg.server.auth_mode = gitcask_config::AuthMode::IntrospectForwarded;
            cfg.auth.introspect.url = issuer;
            cfg.auth.introspect.secret_env = "GITCASK_INITIALIZE_INTROSPECT_SECRET".into();
        })
        .await?;
    let client = reqwest::Client::new();
    let request = body(&src.pinned);
    let direct_url = format!("{}/seed/destination/api/initialize", secure.base_url);
    for token in ["target-only", "source-only"] {
        assert_eq!(
            client
                .post(&direct_url)
                .bearer_auth(token)
                .json(&request)
                .send()
                .await?
                .status(),
            404
        );
    }
    assert_eq!(
        client
            .post(&direct_url)
            .bearer_auth("both")
            .json(&request)
            .send()
            .await?
            .status(),
        201
    );
    let management_url = format!("{}/seed/forwarded/api/initialize", secure.base_url);
    let management = || {
        client
            .post(&management_url)
            .header("X-Gitcask-Principal", "platform")
            .header("X-Gitcask-Write", "1")
            .json(&request)
    };
    assert_eq!(management().send().await?.status(), 401);
    assert_eq!(
        management()
            .header("X-Gitcask-Forward-Secret", "wrong")
            .bearer_auth("both")
            .send()
            .await?
            .status(),
        401
    );
    assert_eq!(
        client
            .post(&management_url)
            .header("X-Gitcask-Forward-Secret", "initialize-proxy")
            .header("X-Gitcask-Principal", "platform")
            .json(&request)
            .send()
            .await?
            .status(),
        403
    );
    assert_eq!(
        management()
            .header("X-Gitcask-Forward-Secret", "initialize-proxy")
            .bearer_auth("invalid-ignored")
            .send()
            .await?
            .status(),
        201
    );
    assert_eq!(
        management()
            .header("X-Gitcask-Forward-Secret", "initialize-proxy")
            .send()
            .await?
            .status(),
        200
    );
    issuer_task.abort();
    Ok(())
}
