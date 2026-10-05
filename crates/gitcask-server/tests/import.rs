//! Real HTTP and Git, synthetic repositories, independent instances over one
//! truth store. These tests are also the process-independent Cloud contract rig.
mod harness;
use anyhow::{Result, ensure};
use gitcask_git::RepoId;
use gitcask_proto::v1::{RefTransaction, RefUpdate};
use harness::{Server, git_in};
use serde_json::{Value, json};
use std::collections::HashMap;

async fn create(server: &Server, name: &str, format: &str) -> Result<()> {
    ensure!(
        reqwest::Client::new()
            .put(format!(
                "{}/fixture/{name}?object_format={format}",
                server.base_url
            ))
            .send()
            .await?
            .status()
            == 201
    );
    Ok(())
}
async fn post(server: &Server, path: &str, body: &Value) -> Result<(u16, Value)> {
    let response = reqwest::Client::new()
        .post(format!("{}{}", server.base_url, path))
        .json(body)
        .send()
        .await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok((
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    ))
}
async fn seed(
    server: &Server,
    format: &str,
    detached: bool,
    lfs_history: bool,
) -> Result<tempfile::TempDir> {
    create(server, "source", format).await?;
    let work = tempfile::tempdir()?;
    git_in(
        work.path(),
        &[
            "init",
            "-q",
            "-b",
            "develop",
            &format!("--object-format={format}"),
        ],
    )?;
    std::fs::write(
        work.path().join("file"),
        if lfs_history {
            "version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 3\n"
        } else {
            "old history"
        },
    )?;
    git_in(work.path(), &["add", "."])?;
    git_in(work.path(), &["commit", "-qm", "first"])?;
    let mut old = git_in(work.path(), &["rev-parse", "HEAD"])?;
    git_in(work.path(), &["branch", "release"])?;
    git_in(work.path(), &["tag", "-a", "v1", "-m", "annotated"])?;
    std::fs::write(work.path().join("file"), "new history")?;
    std::fs::write(
        work.path().join(".gitmodules"),
        "[submodule \"module\"]\n path = module\n url = https://127.0.0.1/not-fetched.git\n",
    )?;
    git_in(work.path(), &["add", "."])?;
    git_in(
        work.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "160000,{},module",
                "f".repeat(if format == "sha1" { 40 } else { 64 })
            ),
        ],
    )?;
    git_in(work.path(), &["commit", "-qm", "second"])?;
    if detached {
        old = git_in(work.path(), &["rev-parse", "HEAD"])?;
        std::fs::write(work.path().join("third"), "branch tip after detached HEAD")?;
        git_in(work.path(), &["add", "."])?;
        git_in(work.path(), &["commit", "-qm", "third"])?;
    }
    git_in(
        work.path(),
        &["push", "-q", &server.repo_url("fixture", "source"), "--all"],
    )?;
    git_in(
        work.path(),
        &[
            "push",
            "-q",
            &server.repo_url("fixture", "source"),
            "--tags",
        ],
    )?;
    let source = server
        .state
        .registry
        .open(&RepoId::new("fixture", "source")?)
        .await?;
    drop(source.sync_refs_only().await?);
    let update = if detached {
        RefUpdate {
            name: "HEAD".into(),
            new_oid: old.trim().into(),
            ..Default::default()
        }
    } else {
        RefUpdate {
            name: "HEAD".into(),
            new_symbolic_target: "refs/heads/develop".into(),
            ..Default::default()
        }
    };
    let result = source
        .publish_ref_update(
            RefTransaction {
                updates: vec![update],
                atomic: true,
                ..Default::default()
            },
            HashMap::new(),
        )
        .await?;
    ensure!(result.per_ref.iter().all(|(_, r)| r.is_ok()));
    Ok(work)
}
async fn resolve(server: &Server) -> Result<Value> {
    let (status, snapshot) = post(
        server,
        "/fixture/target/api/import/resolve",
        &json!({"source":"fixture/source"}),
    )
    .await?;
    ensure!(status == 200, "resolve {status}: {snapshot}");
    Ok(snapshot)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_history_head_tags_independence_and_receipt_after_later_writes() -> Result<()> {
    for format in ["sha1", "sha256"] {
        for detached in [false, true] {
            let server = Server::start().await?;
            let work = seed(&server, format, detached, false).await?;
            create(&server, "target", format).await?;
            let snapshot = resolve(&server).await?;
            ensure!(snapshot["refs"].as_array().unwrap().len() == 3);
            let request = json!({"operation_key":"operation-1","snapshot":snapshot});
            // A source write after resolve must not change the fixed imported refs.
            std::fs::write(work.path().join("later"), "excluded after pin")?;
            git_in(work.path(), &["add", "."])?;
            git_in(work.path(), &["commit", "-qm", "moved source"])?;
            git_in(
                work.path(),
                &[
                    "push",
                    "-q",
                    &server.repo_url("fixture", "source"),
                    "develop",
                ],
            )?;
            let (status, result) = post(&server, "/fixture/target/api/import", &request).await?;
            ensure!(status == 201, "import {status}: {result}");
            ensure!(result["head"] == snapshot["head"]);
            // Fresh server/cache must replay from bucket, independent of source.
            let sibling = server.start_sibling_with(|_| {}).await?;
            ensure!(
                reqwest::Client::new()
                    .delete(format!("{}/fixture/source", server.base_url))
                    .send()
                    .await?
                    .status()
                    == 204
            );
            let clone = tempfile::tempdir()?;
            git_in(
                clone.path(),
                &[
                    "clone",
                    "-q",
                    "--bare",
                    &sibling.repo_url("fixture", "target"),
                    "target",
                ],
            )?;
            let cloned = clone.path().join("target");
            ensure!(
                git_in(&cloned, &["rev-parse", "HEAD"])?.trim()
                    == snapshot["head"]["oid"].as_str().unwrap()
            );
            if detached {
                ensure!(!std::fs::read_to_string(cloned.join("HEAD"))?.starts_with("ref:"));
            } else {
                ensure!(git_in(&cloned, &["symbolic-ref", "HEAD"])?.trim() == "refs/heads/develop");
            }
            git_in(&cloned, &["fsck", "--full", "--no-dangling"])?;
            for r in snapshot["refs"].as_array().unwrap() {
                ensure!(
                    git_in(&cloned, &["rev-parse", r["name"].as_str().unwrap()])?.trim()
                        == r["oid"].as_str().unwrap()
                );
            }
            let target = sibling
                .state
                .registry
                .open(&RepoId::new("fixture", "target")?)
                .await?;
            target.write_checkpoint().await?;
            let oid = snapshot["refs"][0]["oid"].as_str().unwrap();
            let mut delete = RefUpdate {
                name: "refs/heads/develop".into(),
                old_oid: oid.into(),
                new_oid: "0".repeat(oid.len()),
                ..Default::default()
            };
            // Sorted develop is first; normal later target write consumes no receipt.
            let moved = target
                .publish_ref_update(
                    RefTransaction {
                        updates: vec![delete.clone()],
                        atomic: true,
                        ..Default::default()
                    },
                    HashMap::new(),
                )
                .await?;
            ensure!(moved.per_ref.iter().all(|(_, r)| r.is_ok()));
            delete.new_oid = oid.into();
            let (status, replay) = post(&sibling, "/fixture/target/api/import", &request).await?;
            ensure!(
                status == 200 && replay["replayed"] == true && replay["seq"] == result["seq"],
                "replay {status}: {replay}"
            );
            ensure!(
                target
                    .local()
                    .ref_view()?
                    .get("refs/heads/develop")
                    .is_none()
            );
            let receipt = reqwest::Client::new()
                .get(format!(
                    "{}/fixture/target/api/import/receipt",
                    sibling.base_url
                ))
                .query(&[
                    ("operation_key", "operation-1"),
                    ("request_hash", result["request_hash"].as_str().unwrap()),
                ])
                .send()
                .await?;
            ensure!(receipt.status() == 200);
            let mut conflict = request.clone();
            conflict["operation_key"] = json!("another");
            ensure!(
                post(&sibling, "/fixture/target/api/import", &conflict)
                    .await?
                    .0
                    == 409
            );
            let cold = sibling.start_sibling_with(|_| {}).await?;
            let handle = cold
                .state
                .registry
                .open(&RepoId::new("fixture", "target")?)
                .await?;
            drop(handle.sync_refs_only().await?);
            ensure!(
                handle
                    .local()
                    .ref_view()?
                    .get("refs/heads/develop")
                    .is_none()
            );
            if detached {
                ensure!(
                    handle.local().ref_view()?.head_oid().as_deref()
                        == snapshot["head"]["oid"].as_str()
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_cross_instance_race_and_conflict() -> Result<()> {
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, false).await?;
    create(&server, "target", "sha1").await?;
    let request = json!({"operation_key":"race","snapshot":resolve(&server).await?});
    let sibling = server.start_sibling_with(|_| {}).await?;
    let (a, b) = tokio::join!(
        post(&server, "/fixture/target/api/import", &request),
        post(&sibling, "/fixture/target/api/import", &request)
    );
    let (a, b) = (a?, b?);
    ensure!(
        [200, 201].contains(&a.0) && [200, 201].contains(&b.0),
        "{a:?} {b:?}"
    );
    ensure!(a.1["seq"] == b.1["seq"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_lfs_and_unsafe_sources_fail_before_publish() -> Result<()> {
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, true).await?;
    create(&server, "target", "sha1").await?;
    let request = json!({"operation_key":"lfs","snapshot":resolve(&server).await?});
    let (status, result) = post(&server, "/fixture/target/api/import", &request).await?;
    ensure!(status == 422, "{status}: {result}");
    let target = server
        .state
        .registry
        .open(&RepoId::new("fixture", "target")?)
        .await?;
    ensure!(gitcask_wal::is_pristine(&target.manifest()));
    for source in [
        "https://127.0.0.1/repo.git",
        "https://169.254.169.254/repo.git",
        "https://user@example.test/repo.git",
        "http://example.test/repo.git",
        "file:///tmp/repo",
    ] {
        ensure!(
            post(
                &server,
                "/fixture/target/api/import/resolve",
                &json!({"source":source})
            )
            .await?
            .0 == 400,
            "{source}"
        );
    }
    Ok(())
}

async fn call(
    state: std::sync::Arc<gitcask_server::AppState>,
    target: &str,
    request: &Value,
) -> Result<(u16, Value)> {
    use tower::ServiceExt;
    let response = gitcask_server::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/fixture/{target}/api/import"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(request)?))?,
        )
        .await?;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok((
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fault_lost_response_crash_and_roundtrip_budgets() -> Result<()> {
    use gitcask_store::fault::{FaultPlan, FaultStore};
    use std::sync::{Arc, atomic::Ordering};
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, false).await?;
    for target in ["target", "before", "ambiguous", "crash"] {
        create(&server, target, "sha1").await?;
    }
    let request = json!({"operation_key":"fault","snapshot":resolve(&server).await?});
    let cache = tempfile::tempdir()?;
    let mut config = (*server.state.cfg).clone();
    config.cache.dir = cache.path().to_path_buf();
    config.git.commit_graph = false;
    let link = FaultStore::new(server.store.clone(), "import", 42);
    link.set_trace(true);
    let state = gitcask_server::AppState::new(Arc::new(config), link.clone()).await?;
    let source = state
        .registry
        .open(&RepoId::new("fixture", "source")?)
        .await?;
    drop(source.sync_full().await?);
    let target = state
        .registry
        .open(&RepoId::new("fixture", "target")?)
        .await?;
    link.take_trace();
    let before = link.stats().ops.load(Ordering::Relaxed);
    let result = call(state.clone(), "target", &request).await?;
    ensure!(result.0 == 201, "{result:?}");
    let used = link.stats().ops.load(Ordering::Relaxed) - before;
    ensure!(used == 7, "warm import store budget {used}");
    ensure!(link.take_trace().iter().all(|line| !line.contains("list")));
    let before = link.stats().ops.load(Ordering::Relaxed);
    ensure!(call(state.clone(), "target", &request).await?.0 == 200);
    ensure!(link.stats().ops.load(Ordering::Relaxed) - before == 1);
    ensure!(target.manifest().head_seq == 1);
    link.set(FaultPlan {
        fail_once_keys: vec!["put:repos/fixture/before/manifest.pb".into()],
        ..Default::default()
    });
    ensure!(call(state.clone(), "before", &request).await?.0 == 503);
    link.heal();
    let untouched = server
        .state
        .registry
        .open(&RepoId::new("fixture", "before")?)
        .await?;
    drop(untouched.sync_refs_only().await?);
    ensure!(gitcask_wal::is_pristine(&untouched.manifest()));
    ensure!(call(state.clone(), "before", &request).await?.0 == 201);
    link.set(FaultPlan {
        p_err_after: 1.0,
        only_keys: Some(vec!["repos/fixture/ambiguous/manifest.pb".into()]),
        ..Default::default()
    });
    let ambiguous = call(state.clone(), "ambiguous", &request).await?;
    ensure!(ambiguous.0 == 201, "{ambiguous:?}");
    ensure!(link.stats().err_after.load(Ordering::Relaxed) > 0);
    link.heal();
    let cold = server.start_sibling_with(|_| {}).await?;
    let replay = post(&cold, "/fixture/ambiguous/api/import", &request).await?;
    ensure!(replay.0 == 200 && replay.1["seq"] == ambiguous.1["seq"]);
    // Crash the publisher after immutable log PUT but before manifest CAS.
    link.set(FaultPlan {
        panic_once_keys: vec!["put:repos/fixture/crash/manifest.pb".into()],
        ..Default::default()
    });
    ensure!(call(state.clone(), "crash", &request).await?.0 == 500);
    link.heal();
    let crashed = server
        .state
        .registry
        .open(&RepoId::new("fixture", "crash")?)
        .await?;
    drop(crashed.sync_refs_only().await?);
    ensure!(gitcask_wal::is_pristine(&crashed.manifest()));
    let recovered = post(&cold, "/fixture/crash/api/import", &request).await?;
    ensure!(recovered.0 == 201, "{recovered:?}");
    ensure!(recovered.1["seq"].as_u64().is_some_and(|seq| seq > 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_sse_and_target_only_receipt_authorization() -> Result<()> {
    use std::time::Duration;
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, false).await?;
    create(&server, "target", "sha1").await?;
    let request = json!({"operation_key":"permissions","snapshot":resolve(&server).await?});
    for objects in [false, true] {
        let limited = server
            .start_sibling_with(|cfg| {
                if objects {
                    cfg.import.max_objects = 1;
                } else {
                    cfg.import.max_bytes = bytesize::ByteSize::b(16);
                }
            })
            .await?;
        let result = post(&limited, "/fixture/target/api/import", &request).await?;
        ensure!(result.0 == 413, "limit {result:?}");
    }
    let (private, public) = gitcask_server::auth::generate_key_pair_pem()?;
    let secure = server
        .start_sibling_with(|cfg| {
            cfg.server.auth_mode = gitcask_config::AuthMode::Jwt;
            cfg.auth.jwt.public_key = Some(public);
            cfg.auth.jwt.issuer = "import-test".into();
        })
        .await?;
    let token = |scopes: &[&str]| {
        gitcask_server::auth::mint_token(
            &private,
            "import-test",
            None,
            "importer",
            &scopes.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            Duration::from_mins(1),
        )
    };
    let client = reqwest::Client::new();
    let url = format!("{}/fixture/target/api/import", secure.base_url);
    ensure!(client.post(&url).json(&request).send().await?.status() == 401);
    let target_write = token(&["fixture/target:write"])?;
    ensure!(
        client
            .post(&url)
            .bearer_auth(&target_write)
            .json(&request)
            .send()
            .await?
            .status()
            == 404
    );
    let both = token(&["fixture/source:read", "fixture/target:write"])?;
    let resolved = client
        .post(format!("{url}/resolve"))
        .bearer_auth(&both)
        .json(&json!({"source":"fixture/source"}))
        .send()
        .await?;
    ensure!(resolved.status() == 200);
    let target_read = token(&["fixture/target:read"])?;
    let tasks = client
        .get(format!("{}/fixture/target/api/tasks", secure.base_url))
        .bearer_auth(&target_read)
        .send()
        .await?
        .text()
        .await?;
    let records: Value = serde_json::from_str(&tasks)?;
    let task_id = records["recent"][0]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing resolve task"))?;
    let replay = client
        .get(format!(
            "{}/fixture/target/api/tasks/{task_id}",
            secure.base_url
        ))
        .bearer_auth(&target_read)
        .header("accept", "text/event-stream")
        .send()
        .await?
        .text()
        .await?;
    ensure!(
        replay.contains("snapshot_hash")
            && !replay.contains("fixture/source")
            && !replay.contains("refs/heads/develop"),
        "private resolve snapshot leaked through target task stream: {replay}"
    );
    let response = client
        .post(&url)
        .bearer_auth(&both)
        .header("accept", "text/event-stream")
        .json(&request)
        .send()
        .await?;
    ensure!(response.status() == 200);
    let text = response.text().await?;
    ensure!(
        text.contains("event: notice")
            && text.contains("event: task")
            && text.contains("event: result"),
        "{text}"
    );
    // Source grants remain mandatory on import replay, but committed-receipt
    // verification can be done using only read permission on the target.
    ensure!(
        client
            .post(&url)
            .bearer_auth(&target_write)
            .json(&request)
            .send()
            .await?
            .status()
            == 404
    );
    let result = post(&server, "/fixture/target/api/import", &request).await?;
    ensure!(result.0 == 200);
    let read = token(&["fixture/target:read"])?;
    let response = client
        .get(format!("{url}/receipt"))
        .bearer_auth(&read)
        .query(&[
            ("operation_key", "permissions"),
            (
                "request_hash",
                result.1["request_hash"].as_str().unwrap_or_default(),
            ),
        ])
        .send()
        .await?;
    ensure!(response.status() == 200);
    let denied = token(&["fixture/source:read"])?;
    ensure!(
        client
            .get(format!("{url}/receipt"))
            .bearer_auth(&denied)
            .query(&[
                ("operation_key", "permissions"),
                (
                    "request_hash",
                    result.1["request_hash"].as_str().unwrap_or_default()
                )
            ])
            .send()
            .await?
            .status()
            == 404
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_writer_consumes_pristine_during_import_cas_retry() -> Result<()> {
    use gitcask_store::fault::{FaultPlan, FaultStore};
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, false).await?;
    create(&server, "target", "sha1").await?;
    let snapshot = resolve(&server).await?;
    let oid = snapshot["head"]["oid"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let request = json!({"operation_key":"raced-by-writer","snapshot":snapshot});
    let source = server
        .state
        .registry
        .open(&RepoId::new("fixture", "source")?)
        .await?;
    let guard = source.sync_full().await?;
    let target = server
        .state
        .registry
        .open(&RepoId::new("fixture", "target")?)
        .await?;
    drop(target.sync_refs_only().await?);
    let pack = target
        .local()
        .import_pack_from(
            source.local().path(),
            Some(std::slice::from_ref(&oid)),
            gitcask_git::IngestOptions {
                fsck: true,
                max_bytes: Some(1024 * 1024),
                thin: false,
            },
        )
        .await?;
    drop(guard);
    let cache = tempfile::tempdir()?;
    let mut config = (*server.state.cfg).clone();
    config.cache.dir = cache.path().to_path_buf();
    config.wal.cas_max_retries = 20;
    let link = FaultStore::new(server.store.clone(), "import-race", 7);
    let state = gitcask_server::AppState::new(Arc::new(config), link.clone()).await?;
    link.set(FaultPlan {
        delay: Some((Duration::from_millis(100), Duration::from_millis(100))),
        p_cas_fail: 1.0,
        only_keys: Some(vec!["repos/fixture/target/manifest.pb".into()]),
        ..Default::default()
    });
    let importing = tokio::spawn(async move { call(state, "target", &request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while link.stats().cas_fail.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    let written = target
        .publish_push(
            pack,
            RefTransaction {
                updates: vec![RefUpdate {
                    name: "refs/heads/intruder".into(),
                    new_oid: oid.clone(),
                    ..Default::default()
                }],
                atomic: true,
                ..Default::default()
            },
            HashMap::new(),
        )
        .await?;
    ensure!(written.per_ref.iter().all(|(_, r)| r.is_ok()));
    link.heal();
    let refused = importing.await??;
    ensure!(refused.0 == 409, "{refused:?}");
    drop(target.sync_refs_only().await?);
    ensure!(target.manifest().import_receipt.is_none());
    ensure!(
        target
            .local()
            .ref_view()?
            .get("refs/heads/intruder")
            .as_deref()
            == Some(oid.as_str())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_pristine_ttl_does_not_reopen_source_after_remote_commit() -> Result<()> {
    let server = Server::start().await?;
    let _source = seed(&server, "sha1", false, false).await?;
    create(&server, "target", "sha1").await?;
    let request = json!({"operation_key":"ttl-replay","snapshot":resolve(&server).await?});
    let cached = server
        .start_sibling_with(|cfg| cfg.wal.freshness_ttl = std::time::Duration::from_secs(60))
        .await?;
    let target = cached
        .state
        .registry
        .open(&RepoId::new("fixture", "target")?)
        .await?;
    drop(target.sync_refs_only().await?);
    ensure!(gitcask_wal::is_pristine(&target.manifest()));
    let first = post(&server, "/fixture/target/api/import", &request).await?;
    ensure!(first.0 == 201);
    server
        .state
        .registry
        .delete(&RepoId::new("fixture", "source")?)
        .await?;
    let replay = post(&cached, "/fixture/target/api/import", &request).await?;
    ensure!(
        replay.0 == 200 && replay.1["seq"] == first.1["seq"] && replay.1["replayed"] == true,
        "{replay:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_source_with_missing_peel_hint_publishes_verified_tag_metadata() -> Result<()> {
    let server = Server::start().await?;
    let _work = seed(&server, "sha1", false, false).await?;
    create(&server, "target", "sha1").await?;
    let source = server
        .state
        .registry
        .open(&RepoId::new("fixture", "source")?)
        .await?;
    let view = source.local().ref_view()?;
    let tag = view
        .get("refs/tags/v1")
        .ok_or_else(|| anyhow::anyhow!("missing tag"))?;
    let result = source
        .publish_ref_update(
            RefTransaction {
                updates: vec![RefUpdate {
                    name: "refs/tags/v1".into(),
                    old_oid: tag.clone(),
                    new_oid: tag.clone(),
                    ..Default::default()
                }],
                atomic: true,
                ..Default::default()
            },
            HashMap::new(),
        )
        .await?;
    ensure!(result.per_ref.iter().all(|(_, r)| r.is_ok()));
    let cold = server.start_sibling_with(|_| {}).await?;
    let snapshot = resolve(&cold).await?;
    let tag_ref = snapshot["refs"]
        .as_array()
        .and_then(|refs| refs.iter().find(|r| r["name"] == "refs/tags/v1"))
        .ok_or_else(|| anyhow::anyhow!("missing tag snapshot"))?;
    ensure!(tag_ref["peeled"] == "");
    let request = json!({"operation_key":"missing-peel","snapshot":snapshot});
    let imported = post(&cold, "/fixture/target/api/import", &request).await?;
    ensure!(imported.0 == 201, "{imported:?}");
    let replica = server.start_sibling_with(|_| {}).await?;
    let advertisement = git_in(
        _work.path(),
        &[
            "ls-remote",
            &replica.repo_url("fixture", "target"),
            "refs/tags/v1*",
        ],
    )?;
    ensure!(
        advertisement.contains(&tag) && advertisement.contains("refs/tags/v1^{}"),
        "{advertisement}"
    );
    Ok(())
}
