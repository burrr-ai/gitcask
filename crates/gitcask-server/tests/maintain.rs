//! The `maintain` role's pass: checkpoint-if-due (refs-level), compaction,
//! all as tasks.

mod harness;

use gitcask_store::{ObjectStore, ObjectStoreExt, PutBody, PutMode, PutOptions};
use harness::{Server, git, git_in};
use prost::Message;

fn marker_key(owner: &str, repo: &str) -> String {
    gitcask_proto::keys::pending_key(owner, repo)
}

async fn marker_exists(server: &Server, owner: &str, repo: &str) -> anyhow::Result<bool> {
    Ok(server.store.head(&marker_key(owner, repo)).await?.is_some())
}

async fn put_marker(server: &Server, key: &str) -> anyhow::Result<()> {
    server
        .store
        .put(
            key,
            PutBody::Bytes(bytes::Bytes::new()),
            PutOptions::from(PutMode::Overwrite),
        )
        .await?;
    Ok(())
}

async fn pending_count(server: &Server) -> anyhow::Result<usize> {
    use futures::StreamExt;
    let markers = server
        .store
        .list(gitcask_proto::keys::PENDING_DIR, None)
        .collect::<Vec<_>>()
        .await;
    for marker in &markers {
        marker
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(markers.len())
}

async fn object_count(server: &Server, prefix: &str) -> anyhow::Result<usize> {
    use futures::StreamExt;
    let objects = server.store.list(prefix, None).collect::<Vec<_>>().await;
    for object in &objects {
        object
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(objects.len())
}

async fn put_heartbeat(
    server: &Server,
    host: &str,
    age: std::time::Duration,
) -> anyhow::Result<()> {
    let updated_at = std::time::SystemTime::now()
        .checked_sub(age)
        .expect("test heartbeat age is representable");
    let heartbeat = gitcask_proto::v1::MaintainerHeartbeat {
        host: host.to_string(),
        repos: Vec::new(),
        exclude: Vec::new(),
        max_pack_bytes: 0,
        disk: "local".into(),
        started_at: Some(gitcask_proto::time::from_system(updated_at)),
        last_pass_at: Some(gitcask_proto::time::from_system(updated_at)),
        last_unit: String::new(),
        passes: 1,
    };
    server
        .store
        .put(
            &gitcask_proto::keys::maintainer_key(host),
            PutBody::Bytes(heartbeat.encode_to_vec().into()),
            PutOptions::from(PutMode::Overwrite),
        )
        .await?;
    Ok(())
}

async fn put_fsck(
    handle: &gitcask_wal::RepoHandle,
    audited_seq: u64,
    audited_at: std::time::SystemTime,
) -> anyhow::Result<()> {
    let report = gitcask_proto::v1::FsckReport {
        seq: audited_seq,
        at: Some(gitcask_proto::time::from_system(audited_at)),
        audited_seq,
        ..Default::default()
    };
    handle
        .store()
        .put_bytes(
            gitcask_proto::keys::FSCK,
            report.encode_to_vec(),
            PutMode::Overwrite,
        )
        .await?;
    Ok(())
}

fn new_source() -> anyhow::Result<tempfile::TempDir> {
    let source = tempfile::tempdir()?;
    git_in(source.path(), &["init", "-q", "-b", "main"])?;
    git_in(source.path(), &["config", "user.email", "t@t"])?;
    git_in(source.path(), &["config", "user.name", "Tester"])?;
    Ok(source)
}

fn commit_and_push(
    server: &Server,
    source: &std::path::Path,
    owner: &str,
    repo: &str,
    message: &str,
) -> anyhow::Result<()> {
    std::fs::write(source.join("value.txt"), format!("{message}\n"))?;
    git_in(source, &["add", "."])?;
    git_in(source, &["commit", "-q", "-m", message])?;
    let url = server.repo_url(owner, repo);
    git(&["push", "-q", &url, "main"], source)
}

/// Every await is bounded so a hang names the step instead of stalling CI.
macro_rules! step {
    ($name:literal, $e:expr) => {
        tokio::time::timeout(std::time::Duration::from_secs(30), $e)
            .await
            .unwrap_or_else(|_| panic!("step timed out: {}", $name))
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_removes_own_heartbeat() -> anyhow::Result<()> {
    let host = "maintainer-shutdown";
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.roles = vec![gitcask_config::Role::Maintain];
        cfg.maintenance.host = Some(host.to_string());
    })
    .await?;
    let key = gitcask_proto::keys::maintainer_key(host);
    let state = server.state.clone();
    let maintainer = tokio::spawn(async move {
        gitcask_server::maintain::run_loop(state).await;
    });

    step!("wait for heartbeat", async {
        loop {
            if server.store.head(&key).await?.is_some() {
                break Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })?;

    maintainer.abort();
    let _ = maintainer.await;
    gitcask_server::maintain::remove_heartbeat(&server.state).await;
    assert!(server.store.head(&key).await?.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_repo_first_pass_skips_fsck_and_deletes_marker() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
    })
    .await?;
    server.put_repo("schedule", "new").await?;
    let source = new_source()?;
    commit_and_push(&server, source.path(), "schedule", "new", "first")?;

    let id = gitcask_git::RepoId::new("schedule", "new")?;
    let handle = server.state.registry.open(&id).await?;
    let report = gitcask_server::maintain::run_pass(&server.state).await?;

    assert_eq!(report.units, 0, "young repository stays idle: {report:?}");
    assert!(!marker_exists(&server, "schedule", "new").await?);
    assert!(
        gitcask_server::ops::read_fsck(&handle)
            .await
            .map_err(anyhow::Error::msg)?
            .is_none(),
        "first visit must not write fsck.pb"
    );
    assert!(
        server
            .state
            .registry
            .tasks()
            .recent("schedule/new")
            .iter()
            .all(|task| task.kind != "fsck")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_gc_expires_only_stale_instances() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.server.roles = vec![gitcask_config::Role::Maintain];
        cfg.maintenance.heartbeat_ttl = std::time::Duration::from_hours(1);
    })
    .await?;
    put_heartbeat(&server, "stale", std::time::Duration::from_hours(2)).await?;
    put_heartbeat(&server, "recent", std::time::Duration::from_mins(5)).await?;

    let live = gitcask_server::maintain::heartbeats(&server.state).await?;
    assert_eq!(
        live.iter()
            .map(|heartbeat| heartbeat.host.as_str())
            .collect::<Vec<_>>(),
        vec!["recent"]
    );
    assert!(
        server
            .store
            .head(&gitcask_proto::keys::maintainer_key("stale"))
            .await?
            .is_none()
    );
    assert!(
        server
            .store
            .head(&gitcask_proto::keys::maintainer_key("recent"))
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_is_audited_at_the_current_head() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = true;
        cfg.compaction.trigger_packs = 2;
    })
    .await?;
    server.put_repo("schedule", "compact").await?;
    let source = new_source()?;
    commit_and_push(&server, source.path(), "schedule", "compact", "first")?;
    commit_and_push(&server, source.path(), "schedule", "compact", "second")?;

    let first = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(first.compactions, 1, "compaction ran: {first:?}");
    assert_eq!(first.gcs, 1, "GC follows the compaction audit: {first:?}");
    assert!(
        !marker_exists(&server, "schedule", "compact").await?,
        "compaction and its audit reach idle in one pass"
    );
    let id = gitcask_git::RepoId::new("schedule", "compact")?;
    let handle = server.state.registry.open(&id).await?;
    let report = gitcask_server::ops::read_fsck(&handle)
        .await
        .map_err(anyhow::Error::msg)?
        .expect("fsck.pb written after compaction");
    assert_eq!(report.audited_seq, handle.manifest().head_seq);
    assert_eq!(
        gitcask_server::maintain::next_unit(&server.state, &id).await?,
        gitcask_server::maintain::Unit::Idle
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_removes_expired_superseded_packs_logs_and_checkpoints() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = true;
        cfg.compaction.trigger_packs = usize::MAX;
        cfg.compaction.retention_superseded = std::time::Duration::ZERO;
    })
    .await?;
    server.put_repo("gc", "expired").await?;
    let source = new_source()?;
    for index in 1..=10 {
        commit_and_push(
            &server,
            source.path(),
            "gc",
            "expired",
            &format!("push {index}"),
        )?;
        if index == 5 {
            let id = gitcask_git::RepoId::new("gc", "expired")?;
            server
                .state
                .registry
                .open(&id)
                .await?
                .write_checkpoint()
                .await?;
        }
    }

    let id = gitcask_git::RepoId::new("gc", "expired")?;
    let handle = server.state.registry.open(&id).await?;
    let superseded: Vec<_> = handle
        .manifest()
        .packs
        .iter()
        .map(|pack| pack.checksum.clone())
        .collect();
    let compacted = gitcask_server::ops::compact_repo(
        &handle,
        &server.state.cfg,
        gitcask_server::ops::CompactRequest { force: true },
        &gitcask_server::ops::noop_log,
    )
    .await?;
    assert!(
        matches!(
            compacted,
            gitcask_server::ops::CompactOutcome::Published { .. }
        ),
        "{compacted:?}"
    );
    let latest_checkpoint = handle.write_checkpoint().await?;
    let live_pack = handle.manifest().packs[0].clone();

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.compactions, 0, "{report:?}");
    assert_eq!(report.gcs, 1, "{report:?}");
    assert_eq!(
        gitcask_server::maintain::next_unit(&server.state, &id).await?,
        gitcask_server::maintain::Unit::Idle
    );

    let prefix = id.store_prefix();
    for checksum in &superseded {
        for key in [
            gitcask_proto::keys::pack_key(checksum),
            gitcask_proto::keys::idx_key(checksum),
        ] {
            assert!(
                server
                    .store
                    .head(&format!("{prefix}{key}"))
                    .await?
                    .is_none(),
                "superseded object remains: {key}"
            );
        }
    }
    assert!(
        server
            .store
            .head(&format!(
                "{prefix}{}",
                gitcask_proto::keys::pack_key(&live_pack.checksum)
            ))
            .await?
            .is_some(),
        "current pack remains"
    );
    assert!(
        server
            .store
            .head(&format!("{prefix}{}", latest_checkpoint.key))
            .await?
            .is_some(),
        "latest checkpoint remains"
    );
    assert!(
        server
            .store
            .head(&format!(
                "{prefix}{}",
                gitcask_proto::keys::checkpoint_key(5)
            ))
            .await?
            .is_none(),
        "old checkpoint was collected"
    );
    assert!(
        server
            .store
            .head(&format!(
                "{prefix}{}",
                gitcask_proto::keys::log_segment_key(1)
            ))
            .await?
            .is_none(),
        "folded log was collected"
    );
    assert!(
        server
            .store
            .head(&format!("{prefix}{}", gitcask_proto::keys::GC))
            .await?
            .is_some(),
        "durable cursor records the completed unit"
    );

    let clone_parent = tempfile::tempdir()?;
    let clone = clone_parent.path().join("clone");
    git(
        &[
            "clone",
            "-q",
            &server.repo_url("gc", "expired"),
            clone.to_str().expect("temporary path is UTF-8"),
        ],
        clone_parent.path(),
    )?;
    git_in(&clone, &["fsck", "--connectivity-only"])?;
    let fsck = gitcask_server::ops::read_fsck(&handle)
        .await
        .map_err(anyhow::Error::msg)?
        .expect("compaction audit exists");
    assert_eq!(fsck.missing_total, 0);
    assert_eq!(fsck.problems, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_expires_and_rebuilds_shared_archive_cache() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = true;
        cfg.compaction.trigger_packs = usize::MAX;
        cfg.cache.shared_retention = std::time::Duration::ZERO;
    })
    .await?;
    server.put_repo("cache", "retention").await?;
    let source = new_source()?;
    commit_and_push(
        &server,
        source.path(),
        "cache",
        "retention",
        "populate cache",
    )?;

    let client = reqwest::Client::new();
    let archive_url = format!("{}/cache/retention/api/archive/main", server.base_url);
    let response = client.get(&archive_url).send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let id = gitcask_git::RepoId::new("cache", "retention")?;
    let cache_prefix = format!(
        "{}{}",
        id.store_prefix(),
        gitcask_proto::keys::ARCHIVE_CACHE_DIR
    );
    assert_eq!(object_count(&server, &cache_prefix).await?, 1);

    let handle = server.state.registry.open(&id).await?;
    let compacted = gitcask_server::ops::compact_repo(
        &handle,
        &server.state.cfg,
        gitcask_server::ops::CompactRequest { force: true },
        &gitcask_server::ops::noop_log,
    )
    .await?;
    assert!(matches!(
        compacted,
        gitcask_server::ops::CompactOutcome::Published { .. }
    ));
    handle.write_checkpoint().await?;

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.gcs, 1, "GC follows compaction and fsck: {report:?}");
    assert_eq!(
        object_count(&server, &cache_prefix).await?,
        0,
        "expired archive cache object was conditionally deleted"
    );

    let rebuilt = client.get(&archive_url).send().await?;
    assert_eq!(rebuilt.status(), reqwest::StatusCode::OK);
    assert_eq!(
        object_count(&server, &cache_prefix).await?,
        1,
        "the next request rebuilt the immutable archive"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_replans_instead_of_deleting_from_a_stale_manifest() -> anyhow::Result<()> {
    use gitcask_store::fault::{FaultPlan, FaultStore};

    let truth: gitcask_store::DynStore = gitcask_store::memory::MemoryStore::shared();
    let link = FaultStore::new(truth.clone(), "gc-manifest-race", 1);
    let cache = tempfile::tempdir()?;
    let mut cfg = gitcask_config::Config::default();
    cfg.cache.dir = cache.path().to_path_buf();
    cfg.store.backend = gitcask_config::StoreBackend::Memory;
    cfg.store.bucket = "test".into();
    cfg.wal.snapshot_every_entries = 0;
    cfg.wal.checkpoint_interval = std::time::Duration::ZERO;
    cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
    cfg.compaction.retention_superseded = std::time::Duration::ZERO;
    let state = gitcask_server::AppState::new(
        std::sync::Arc::new(cfg),
        link.clone() as gitcask_store::DynStore,
    )
    .await?;
    let id = gitcask_git::RepoId::new("gc", "manifest-race")?;
    let handle = state
        .registry
        .create(&id, gitcask_git::ObjectFormat::Sha1)
        .await?;
    handle
        .publish_ref_update(Default::default(), Default::default())
        .await?;
    handle.write_checkpoint().await?;

    // Hold GC after it listed the old manifest's folded log. A concurrent
    // publish commits a new manifest generation before GC reaches its guard.
    link.set(FaultPlan {
        delay_after: Some(std::time::Duration::from_millis(200)),
        only_keys: Some(vec![gitcask_proto::keys::LOG_DIR.to_string()]),
        ..Default::default()
    });
    let gc = tokio::spawn(gitcask_server::gc::collect(
        handle.clone(),
        std::time::Duration::ZERO,
        std::time::Duration::from_hours(30 * 24),
        std::time::Duration::from_secs(1),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    handle
        .publish_ref_update(Default::default(), Default::default())
        .await?;
    let error = gc
        .await?
        .expect_err("manifest generation changed during the GC plan");
    assert!(error.contains("manifest changed"), "{error}");
    link.set(FaultPlan::default());

    let prefix = id.store_prefix();
    assert!(
        truth
            .head(&format!(
                "{prefix}{}",
                gitcask_proto::keys::log_segment_key(1)
            ))
            .await?
            .is_some(),
        "the stale plan must not delete before manifest revalidation"
    );
    assert_eq!(handle.manifest().head_seq, 2);

    let outcome = gitcask_server::gc::collect(
        handle.clone(),
        std::time::Duration::ZERO,
        std::time::Duration::from_hours(30 * 24),
        std::time::Duration::from_secs(1),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    assert_eq!(outcome.logs, 1);
    assert!(
        truth
            .head(&format!(
                "{prefix}{}",
                gitcask_proto::keys::log_segment_key(2)
            ))
            .await?
            .is_some(),
        "the current manifest's tail remains"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aged_audit_requires_a_newer_push() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;
    server.put_repo("schedule", "aged").await?;
    let id = gitcask_git::RepoId::new("schedule", "aged")?;
    let handle = server.state.registry.open(&id).await?;
    put_fsck(&handle, 0, std::time::SystemTime::UNIX_EPOCH).await?;

    let source = new_source()?;
    commit_and_push(&server, source.path(), "schedule", "aged", "first")?;
    let first = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(
        first.units, 1,
        "new push makes the aged audit due: {first:?}"
    );
    let audited = gitcask_server::ops::read_fsck(&handle)
        .await
        .map_err(anyhow::Error::msg)?
        .expect("fsck.pb rewritten");
    assert_eq!(audited.audited_seq, handle.manifest().head_seq);
    let fsck_tasks = server
        .state
        .registry
        .tasks()
        .recent("schedule/aged")
        .iter()
        .filter(|task| task.kind == "fsck")
        .count();

    put_marker(&server, &marker_key("schedule", "aged")).await?;
    let second = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(second.units, 0, "no push since the audit: {second:?}");
    assert!(!marker_exists(&server, "schedule", "aged").await?);
    assert_eq!(
        server
            .state
            .registry
            .tasks()
            .recent("schedule/aged")
            .iter()
            .filter(|task| task.kind == "fsck")
            .count(),
        fsck_tasks,
        "rewriting only the marker must not start another fsck"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pass_checkpoints_due_repos_refs_level_and_reports_tasks() -> anyhow::Result<()> {
    // Writer front: count trigger off, so nothing auto-checkpoints on push.
    let front = step!("start front", Server::start())?;
    step!("put repo", front.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    for i in 0..3 {
        std::fs::write(src.path().join(format!("f{i}.txt")), format!("{i}\n"))?;
        git_in(src.path(), &["add", "."])?;
        git_in(src.path(), &["commit", "-q", "-m", &format!("c{i}")])?;
        git(
            &["push", "-q", &front.repo_url("o", "r"), "main"],
            src.path(),
        )?;
    }
    assert!(marker_exists(&front, "o", "r").await?);
    let overview: serde_json::Value =
        serde_json::from_str(&front.get_text("/o/r/api/overview", &[]).await?)?;
    assert_eq!(overview["pending"], true);
    let m = step!(
        "open on front",
        front
            .state
            .registry
            .open(&gitcask_git::RepoId::new("o", "r")?)
    )?
    .manifest();
    assert_eq!(m.head_seq, 3);
    assert!(
        m.checkpoint.is_none(),
        "no checkpoint yet: {:?}",
        m.checkpoint
    );

    // Maintainer: age trigger (1 ms). Checkpointing stays refs-only.
    let maint = step!(
        "start maintainer",
        front.start_sibling_with(|c| {
            c.server.roles = vec![gitcask_config::Role::Maintain];
            c.wal.snapshot_every_entries = 0;
            c.wal.checkpoint_interval = std::time::Duration::from_millis(1);
            c.compaction.enabled = false;
        })
    )?;
    let h = step!(
        "open on maintainer",
        maint
            .state
            .registry
            .open(&gitcask_git::RepoId::new("o", "r")?)
    )?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let report = step!(
        "maintain pass 1",
        gitcask_server::maintain::run_pass(&maint.state)
    )?;
    assert_eq!(report.repos, 1);
    assert_eq!(report.checkpoints, 1, "{report:?}");
    assert!(marker_exists(&front, "o", "r").await?);

    // Manifest folded; the task is discoverable on the maintainer.
    let m = h.manifest();
    assert_eq!(m.checkpoint.as_ref().map(|c| c.seq), Some(3));
    assert!(m.log_segments.is_empty());
    assert!(
        h.local().packs()?.is_empty(),
        "refs-level: no pack downloaded"
    );
    let tasks = step!("tasks list", maint.get_text("/o/r/api/tasks", &[]))?;
    assert!(tasks.contains("\"checkpoint\""), "{tasks}");
    assert!(
        tasks.contains("\"trigger\":\"age\"") || tasks.contains("age"),
        "{tasks}"
    );

    // Second pass: no compaction means the young repository is idle, so the
    // marker is consumed without downloading packs for fsck.
    let report = step!(
        "maintain pass 2",
        gitcask_server::maintain::run_pass(&maint.state)
    )?;
    assert_eq!(report.markers, 1);
    assert_eq!(report.checkpoints, 0);
    assert!(!marker_exists(&front, "o", "r").await?);

    // The front sees the checkpoint and a fresh instance cold-starts from it.
    let cold = step!("start cold", front.start_sibling_with(|_| {}))?;
    let refs = step!("cold ls-remote", cold.ls_remote("o", "r"))?;
    let head = git_in(src.path(), &["rev-parse", "HEAD"])?;
    assert!(refs.contains(head.trim()), "{refs}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_during_a_pass_leaves_the_replaced_marker() -> anyhow::Result<()> {
    use gitcask_store::fault::{FaultPlan, FaultStore};

    let truth: gitcask_store::DynStore = gitcask_store::memory::MemoryStore::shared();
    let link = FaultStore::new(truth.clone(), "maintain-race", 1);
    link.set(FaultPlan {
        delay: Some((
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(200),
        )),
        only_keys: Some(vec![gitcask_proto::keys::FSCK.to_string()]),
        ..Default::default()
    });
    let cache = tempfile::tempdir()?;
    let mut cfg = gitcask_config::Config::default();
    cfg.cache.dir = cache.path().to_path_buf();
    cfg.store.backend = gitcask_config::StoreBackend::Memory;
    cfg.wal.snapshot_every_entries = 0;
    cfg.wal.checkpoint_interval = std::time::Duration::ZERO;
    cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
    cfg.compaction.enabled = false;
    cfg.maintenance.checkpoints = false;
    cfg.maintenance.fsck_interval = std::time::Duration::from_hours(1);
    let state =
        gitcask_server::AppState::new(std::sync::Arc::new(cfg), link as gitcask_store::DynStore)
            .await?;
    let id = gitcask_git::RepoId::new("o", "race")?;
    let handle = state
        .registry
        .create(&id, gitcask_git::ObjectFormat::Sha1)
        .await?;
    handle
        .publish_ref_update(
            gitcask_proto::v1::RefTransaction::default(),
            std::collections::HashMap::default(),
        )
        .await?;
    put_fsck(&handle, 0, std::time::SystemTime::UNIX_EPOCH).await?;
    let key = marker_key("o", "race");
    let listed_version = truth.head(&key).await?.expect("marker after push").version;
    let pass = tokio::spawn({
        let state = state.clone();
        async move { gitcask_server::maintain::run_pass(&state).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !state
            .registry
            .tasks()
            .running_all()
            .iter()
            .any(|task| task.kind == "fsck")
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await?;
    handle
        .publish_ref_update(
            gitcask_proto::v1::RefTransaction::default(),
            std::collections::HashMap::default(),
        )
        .await?;
    let report = pass.await??;
    assert!(report.units >= 1, "fsck ran: {report:?}");
    let marker = truth
        .head(&key)
        .await?
        .expect("the newer push marker must remain");
    assert_ne!(marker.version, listed_version);
    assert_eq!(handle.manifest().head_seq, 2);
    Ok(())
}

#[tokio::test]
async fn malformed_and_valid_markers_are_both_consumed() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;
    server.put_repo("o", "r").await?;
    put_marker(&server, "pending/garbage").await?;
    put_marker(&server, &marker_key("o", "r")).await?;

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.markers, 2, "{report:?}");
    assert_eq!(report.repos, 1, "pass continued after garbage: {report:?}");
    assert_eq!(pending_count(&server).await?, 0);
    Ok(())
}

#[tokio::test]
async fn marker_for_a_deleted_repo_is_removed() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;
    let id = gitcask_git::RepoId::new("o", "gone")?;
    server
        .state
        .registry
        .create(&id, gitcask_git::ObjectFormat::Sha1)
        .await?;
    server.state.registry.delete(&id).await?;
    put_marker(&server, &marker_key("o", "gone")).await?;

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.markers, 1);
    assert_eq!(report.repos, 0);
    assert_eq!(pending_count(&server).await?, 0);
    Ok(())
}

#[tokio::test]
async fn pass_stops_at_the_configured_repository_limit() -> anyhow::Result<()> {
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.max_repos_per_pass = 2;
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;
    for index in 0..5 {
        let name = format!("r{index}");
        server
            .state
            .registry
            .create(
                &gitcask_git::RepoId::new("o", &name)?,
                gitcask_git::ObjectFormat::Sha1,
            )
            .await?;
        put_marker(&server, &marker_key("o", &name)).await?;
    }

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.markers, 2, "{report:?}");
    assert_eq!(report.repos, 2, "{report:?}");
    assert_eq!(pending_count(&server).await?, 3);
    Ok(())
}

#[tokio::test]
async fn pending_list_consumes_more_than_one_s3_sized_page() -> anyhow::Result<()> {
    const MARKERS: usize = 1_005;
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.max_repos_per_pass = MARKERS;
    })
    .await?;
    for index in 0..MARKERS {
        put_marker(&server, &format!("pending/o/missing-{index:04}")).await?;
    }

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.markers, MARKERS);
    assert_eq!(pending_count(&server).await?, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_workers_consume_fifty_unique_markers() -> anyhow::Result<()> {
    const MARKERS: usize = 50;
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.workers = 4;
        cfg.maintenance.max_repos_per_pass = MARKERS;
        cfg.maintenance.checkpoints = false;
        cfg.compaction.enabled = false;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;
    for index in 0..MARKERS {
        let id = gitcask_git::RepoId::new("parallel", &format!("r{index:02}"))?;
        server
            .state
            .registry
            .create(&id, gitcask_git::ObjectFormat::Sha1)
            .await?;
        put_marker(&server, &marker_key(id.owner(), id.name())).await?;
    }

    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.markers, MARKERS, "{report:?}");
    assert_eq!(report.repos, MARKERS, "{report:?}");
    assert_eq!(pending_count(&server).await?, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_repo_reaches_idle_while_checkpoint_only_repos_yield() -> anyhow::Result<()> {
    const MARKERS: usize = 30;
    let server = Server::start_with_tweak(|cfg| {
        cfg.maintenance.workers = 4;
        cfg.maintenance.max_repos_per_pass = MARKERS;
        cfg.wal.snapshot_every_entries = 0;
        cfg.wal.checkpoint_interval = std::time::Duration::from_millis(50);
        cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
        cfg.compaction.enabled = true;
        cfg.compaction.trigger_packs = 2;
        cfg.maintenance.fsck_interval = std::time::Duration::ZERO;
    })
    .await?;

    for index in 0..MARKERS - 1 {
        let id = gitcask_git::RepoId::new("fair", &format!("checkpoint-{index:02}"))?;
        let handle = server
            .state
            .registry
            .create(&id, gitcask_git::ObjectFormat::Sha1)
            .await?;
        handle
            .publish_ref_update(
                gitcask_proto::v1::RefTransaction::default(),
                std::collections::HashMap::new(),
            )
            .await?;
    }

    server.put_repo("fair", "hot").await?;
    let source = tempfile::tempdir()?;
    git_in(source.path(), &["init", "-q", "-b", "main"])?;
    git_in(source.path(), &["config", "user.email", "t@t"])?;
    git_in(source.path(), &["config", "user.name", "Tester"])?;
    for index in 0..2 {
        std::fs::write(source.path().join("hot.txt"), format!("{index}\n"))?;
        git_in(source.path(), &["add", "."])?;
        git_in(
            source.path(),
            &["commit", "-q", "-m", &format!("hot {index}")],
        )?;
        git(
            &["push", "-q", &server.repo_url("fair", "hot"), "main"],
            source.path(),
        )?;
    }

    let hot = gitcask_git::RepoId::new("fair", "hot")?;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        matches!(
            gitcask_server::maintain::next_unit(&server.state, &hot).await?,
            gitcask_server::maintain::Unit::Checkpoint(_)
        ),
        "checkpoint remains the first priority"
    );
    let report = gitcask_server::maintain::run_pass(&server.state).await?;
    assert_eq!(report.repos, MARKERS, "{report:?}");
    assert_eq!(report.compactions, 1, "one compaction unit: {report:?}");
    assert_eq!(
        server
            .state
            .registry
            .tasks()
            .recent("fair/hot")
            .iter()
            .filter(|task| task.kind == "compact")
            .count(),
        1,
        "one worker must own the repository's compaction unit"
    );
    assert!(
        report.checkpoints >= MARKERS,
        "every repository checkpointed: {report:?}"
    );
    assert_eq!(
        gitcask_server::maintain::next_unit(&server.state, &hot).await?,
        gitcask_server::maintain::Unit::Idle,
        "hot repository must finish its planner in the same pass"
    );
    assert!(!marker_exists(&server, "fair", "hot").await?);
    assert_eq!(
        pending_count(&server).await?,
        MARKERS - 1,
        "checkpoint-only repositories keep their markers for the next pass"
    );
    Ok(())
}

/// The weekly `fsck` unit records missing objects at fsck.pb and reports the
/// repository as damaged without changing the WAL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_unit_records_missing_objects() -> anyhow::Result<()> {
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::from_secs(3600);
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    let c1 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = gitcask_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    put_fsck(&h, h.manifest().head_seq, std::time::SystemTime::UNIX_EPOCH).await?;
    std::fs::write(src.path().join("b.txt"), "the blob the import dropped\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let tree2 = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let blob2 = git_in(src.path(), &["rev-parse", "HEAD:b.txt"])?
        .trim()
        .to_string();
    // The hole: publish commit 2 + its tree WITHOUT the new blob (a pack that is
    // not the closure of the ref), then move main onto it — exactly the import's
    // mistake, which receive-pack's connectivity check would have refused.
    let holes = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", holes.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c2}\n{tree2}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    step!("sync", h.sync_full())?;
    step!(
        "add hole pack",
        h.add_pack(
            &holes.path().join(format!("pack-{sha}.pack")),
            &holes.path().join(format!("pack-{sha}.idx")),
            0
        )
    )?;
    step!("sync2", h.sync_full())?;
    let txn = gitcask_proto::v1::RefTransaction {
        updates: vec![gitcask_proto::v1::RefUpdate {
            name: "refs/heads/main".into(),
            old_oid: c1.clone(),
            new_oid: c2.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "move main",
        h.publish_push_synced(None, txn, Default::default())
    )?;

    // Pass 1: the old audit plus newer WAL activity makes fsck due; fsck.pb
    // lists the blob and the unit succeeds (a finding, not a failure).
    let unit = step!(
        "plan 1",
        gitcask_server::maintain::next_unit(&server.state, &id)
    )?;
    assert!(
        matches!(unit, gitcask_server::maintain::Unit::Fsck(_)),
        "{unit:?}"
    );
    let report = step!("pass 1", gitcask_server::maintain::run_pass(&server.state))?;
    assert_eq!(report.units, 1, "repository audited: {report:?}");
    let f = gitcask_server::ops::read_fsck(&h)
        .await
        .unwrap()
        .expect("fsck.pb written");
    assert_eq!(f.missing, vec![blob2.clone()], "{f:?}");
    assert_eq!(f.repaired_seq, 0);

    // The finding is reported; no automatic mutation is planned.
    let unit = step!(
        "plan after finding",
        gitcask_server::maintain::next_unit(&server.state, &id)
    )?;
    assert_eq!(unit, gitcask_server::maintain::Unit::Idle, "{unit:?}");

    Ok(())
}

/// A push whose pack references an object the server lacks (the client
/// believes the server has it) is refused with the reason ON EVERY REF —
/// `unpack ng` alone made git print "remote failed to report status" and the
/// server logged nothing (prod 2026-08-21 03:28Z, the 1,952-blob hole).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connectivity_failure_is_reported_per_ref_not_as_remote_failure() -> anyhow::Result<()> {
    let server = step!("start", Server::start())?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    std::fs::write(src.path().join("b.txt"), "two\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "two"])?;
    let c2 = git_in(src.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let blob2 = git_in(src.path(), &["rev-parse", "HEAD:b.txt"])?
        .trim()
        .to_string();
    // Make the client believe the server already has commit 2: a second remote ref
    // in the advertisement. We fake it by pushing only the commit + tree via a
    // ref the server accepts without connectivity (a tag on a pack that lacks the
    // blob is refused too) — so instead feed receive-pack a thin pack directly.
    // Simplest faithful reproduction: push main with `--no-thin` disabled and the
    // blob object deleted from the client's own odb *after* git decided it is
    // unchanged... Too brittle. Use the server API: publish the commit+tree pack
    // (no blob) and advertise `refs/heads/x` at c2; then `git push main` sends
    // zero objects (c2 is "already there") and the server's connectivity check
    // trips on the blob.
    let holes = tempfile::tempdir()?;
    let tree2 = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args(["pack-objects", &format!("{}/pack", holes.path().display())])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{c2}\n{tree2}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let id = gitcask_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync_full())?;
    step!(
        "add hole pack",
        h.add_pack(
            &holes.path().join(format!("pack-{sha}.pack")),
            &holes.path().join(format!("pack-{sha}.idx")),
            0
        )
    )?;
    step!("sync2", h.sync_full())?;
    let txn = gitcask_proto::v1::RefTransaction {
        updates: vec![gitcask_proto::v1::RefUpdate {
            name: "refs/heads/x".into(),
            old_oid: String::new(),
            new_oid: c2.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    step!(
        "advertise x",
        h.publish_push_synced(None, txn, Default::default())
    )?;
    // A new commit on top whose tree still references the missing blob (b.txt
    // unchanged): git sends commit 3 + its root tree, the server walks into b.txt.
    std::fs::write(src.path().join("a.txt"), "three\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "three"])?;

    let out = std::process::Command::new("git")
        .args(["push", "--porcelain", &server.repo_url("o", "r"), "main"])
        .current_dir(src.path())
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "must be refused: {text}");
    assert!(
        !text.contains("remote failure") && !text.contains("failed to report status"),
        "git must see a proper report: {text}"
    );
    assert!(
        text.contains("refs/heads/main") && text.contains("connectivity") && text.contains(&blob2),
        "per-ref reason names the oid: {text}"
    );
    Ok(())
}

/// A pack published without its `.rev` (git < 2.41 wrote none; a large repository's whole
/// serving copy had none, 2.85 s per fetch — the original large-repository measurements) gets one
/// from the maintainer: built where the pack is local, uploaded as the
/// side-file, advertised in the manifest (`has_rev`) so every other host
/// downloads it on its next sync instead of rebuilding it per `pack-objects`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintainer_builds_and_publishes_missing_rev_indexes() -> anyhow::Result<()> {
    use gitcask_server::maintain::{Unit, next_unit};
    let server = step!(
        "start",
        Server::start_with_tweak(|c| {
            c.maintenance.checkpoints = false;
            c.compaction.enabled = false;
            c.maintenance.fsck_interval = std::time::Duration::ZERO;
        })
    )?;
    step!("put repo", server.put_repo("o", "r"))?;
    let src = tempfile::tempdir()?;
    git_in(src.path(), &["init", "-q", "-b", "main"])?;
    git_in(src.path(), &["config", "user.email", "t@t"])?;
    git_in(src.path(), &["config", "user.name", "Tester"])?;
    std::fs::write(src.path().join("a.txt"), "one\n")?;
    git_in(src.path(), &["add", "."])?;
    git_in(src.path(), &["commit", "-q", "-m", "one"])?;
    git(
        &["push", "-q", &server.repo_url("o", "r"), "main"],
        src.path(),
    )?;
    let id = gitcask_git::RepoId::new("o", "r")?;
    let h = step!("open", server.state.registry.open(&id))?;
    step!("sync", h.sync_full())?;
    // Push packs (gix ingest) carry no .rev and need none: below
    // REV_INDEX_MIN_OBJECTS the maintainer leaves them alone.
    assert!(
        h.manifest().packs.iter().all(|p| !p.has_rev),
        "{:?}",
        h.manifest().packs
    );
    assert_eq!(
        step!("idle (small packs)", next_unit(&server.state, &id))?,
        Unit::Idle
    );

    // A legacy-shaped pack: pack-objects to a file with reverse indexes off (no .rev).
    let legacy = tempfile::tempdir()?;
    let tree = git_in(src.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let out = std::process::Command::new("git")
        .current_dir(src.path())
        .args([
            "-c",
            "pack.writeReverseIndex=false",
            "pack-objects",
            &format!("{}/pack", legacy.path().display()),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(format!("{tree}\n").as_bytes())?;
            c.wait_with_output()
        })?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!legacy.path().join(format!("pack-{sha}.rev")).exists());
    step!(
        "add legacy pack",
        h.add_pack(
            &legacy.path().join(format!("pack-{sha}.pack")),
            &legacy.path().join(format!("pack-{sha}.idx")),
            0
        )
    )?;
    step!("sync2", h.sync_full())?;
    assert!(
        h.manifest()
            .packs
            .iter()
            .any(|p| p.checksum == sha && !p.has_rev),
        "{:?}",
        h.manifest().packs
    );

    // Another host installs the pack as it is (no .rev) before the unit runs.
    let other = step!(
        "start other",
        server.start_sibling_with(|c| {
            c.server.roles = vec![gitcask_config::Role::Serve];
        })
    )?;
    let h2 = step!("open other", other.state.registry.open(&id))?;
    step!("sync other", h2.sync_full())?;
    let rev2 = h2
        .local()
        .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
        .with_extension("rev");
    assert!(!rev2.exists());

    // The unit (what the planner would emit for a ≥ REV_INDEX_MIN_OBJECTS pack):
    // build locally, upload the side-file, CAS the manifest.
    let mut params = std::collections::HashMap::new();
    params.insert("pack".to_string(), sha.clone());
    let task = gitcask_server::ops::start(server.state.clone(), id.clone(), "rev-index", params)
        .await
        .map_err(|_| anyhow::anyhow!("rev-index op did not start"))?;
    assert!(task.wait_done(std::time::Duration::from_secs(60)).await);
    assert!(
        matches!(task.outcome(), Some(Ok(_))),
        "{:?}",
        task.outcome()
    );
    assert!(
        h.local()
            .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
            .with_extension("rev")
            .exists()
    );
    step!("sync3", h.sync_full())?;
    let p = h
        .manifest()
        .packs
        .iter()
        .find(|p| p.checksum == sha)
        .cloned()
        .unwrap();
    assert!(p.has_rev, "advertised in the manifest: {p:?}");
    assert!(
        gitcask_store::ObjectStore::head(h.store(), &gitcask_proto::keys::rev_key(&sha))
            .await?
            .is_some(),
        "uploaded as the side-file"
    );
    assert_eq!(step!("idle", next_unit(&server.state, &id))?, Unit::Idle);

    // The other host, pack already installed, picks the side-file up on its
    // next sync (the manifest revision moved) — the fleet converges.
    step!("sync other 2", h2.sync_full())?;
    assert!(
        rev2.exists(),
        "installed pack gets the newly advertised side-file on sync"
    );
    assert_eq!(
        std::fs::read(&rev2)?,
        std::fs::read(
            h.local()
                .pack_path(&gix_hash::ObjectId::from_hex(sha.as_bytes())?)
                .with_extension("rev")
        )?
    );
    Ok(())
}
