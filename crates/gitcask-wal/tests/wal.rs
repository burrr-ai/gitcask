//! Integration tests for gitcask-wal.
//!
//! Uses MemoryStore + real LocalRepo tempdir + upstream git to create
//! objects/packs.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use gitcask_git::{IngestOptions, ObjectFormat, RepoId};
use gitcask_proto::v1::{EntryKind, RefTransaction, RefUpdate};
use gitcask_store::ObjectStore;
use gitcask_store::fault::{FaultPlan, FaultStore};
use gitcask_store::memory::MemoryStore;
use gitcask_wal::Registry;

use std::io::Write;

// ---- test helpers ----

struct WorkRepo {
    dir: tempfile::TempDir,
}

impl WorkRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        WorkRepo { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn commit(&self, msg: &str, content: &str) -> String {
        let fname = format!("file_{}.txt", msg.replace(' ', "_"));
        std::fs::write(self.path().join(&fname), content).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(self.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(self.path())
            .output()
            .unwrap();
        self.head()
    }

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(self.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[allow(dead_code)]
    fn rev_parse(&self, ref_name: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", ref_name])
            .current_dir(self.path())
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Create a pack containing all objects reachable from HEAD.
    fn create_pack(&self) -> Vec<u8> {
        let head = self.head();
        let revs = format!("{head}\n");
        let mut child = Command::new("git")
            .args(["pack-objects", "--stdout", "--revs"])
            .current_dir(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(revs.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "pack-objects failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// Create a pack containing objects reachable from `head` but not from `base`.
    fn create_incremental_pack(&self, head: &str, base: &str) -> Vec<u8> {
        // Use rev-list to enumerate objects, pipe to pack-objects.
        let rev_list = Command::new("git")
            .args(["rev-list", "--objects", head, "--not", base])
            .current_dir(self.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pack = Command::new("git")
            .args(["pack-objects", "--stdout"])
            .current_dir(self.path())
            .stdin(rev_list.stdout.unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = pack.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "pack-objects failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }
}

fn make_txn(updates: Vec<(&str, &str, &str)>) -> RefTransaction {
    RefTransaction {
        updates: updates
            .into_iter()
            .map(|(name, old, new)| RefUpdate {
                name: name.to_string(),
                old_oid: old.to_string(),
                new_oid: new.to_string(),
                new_symbolic_target: String::new(),
                new_peeled: String::new(),
            })
            .collect(),
        push_options: vec![],
        atomic: true,
    }
}

fn make_config(cache_dir: &Path, batch_window_ms: u64) -> gitcask_config::Config {
    let mut cfg = gitcask_config::Config::default();
    cfg.cache.dir = cache_dir.to_path_buf();
    cfg.wal.batch_window = Duration::from_millis(batch_window_ms);
    cfg.wal.freshness_ttl = Duration::ZERO;
    cfg.wal.fsck_objects = false;
    cfg.wal.check_connectivity = false;
    cfg.wal.snapshot_every_entries = 0; // disable auto checkpoint in most tests
    cfg.wal.checkpoint_interval = Duration::ZERO;
    cfg.wal.checkpoint_tail_bytes = gitcask_config::ByteSize::b(0);
    cfg.store.bucket = "test".to_string();
    cfg
}

async fn ingest_pack_data(
    handle: &gitcask_wal::RepoHandle,
    pack_bytes: Vec<u8>,
) -> Option<gitcask_git::IngestedPack> {
    let cursor = std::io::Cursor::new(pack_bytes);
    handle
        .local()
        .ingest_pack(
            cursor,
            IngestOptions {
                fsck: false,
                max_bytes: None,
                thin: false,
            },
        )
        .await
        .unwrap()
}

fn repo_id(owner: &str, name: &str) -> RepoId {
    RepoId::new(owner, name).unwrap()
}

// ---- tests ----

#[tokio::test]
async fn test_create_and_three_pushes() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 0);
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "repo1");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // First push
    let work = WorkRepo::new();
    let c1 = work.commit("first", "hello");
    let pack1 = work.create_pack();
    let ingested1 = ingest_pack_data(&handle, pack1).await.unwrap();
    let txn1 = make_txn(vec![("refs/heads/main", "", &c1)]);
    let result1 = handle
        .publish_push(Some(ingested1), txn1, HashMap::new())
        .await
        .unwrap();
    assert_eq!(result1.seq, 1);
    assert!(result1.per_ref.iter().all(|(_, r)| r.is_ok()));

    // Second push
    let c2 = work.commit("second", "world");
    let pack2 = work.create_incremental_pack(&c2, &c1);
    let ingested2 = ingest_pack_data(&handle, pack2).await.unwrap();
    let txn2 = make_txn(vec![("refs/heads/main", &c1, &c2)]);
    let result2 = handle
        .publish_push(Some(ingested2), txn2, HashMap::new())
        .await
        .unwrap();
    assert_eq!(result2.seq, 2);
    assert!(result2.per_ref.iter().all(|(_, r)| r.is_ok()));

    // Third push
    let c3 = work.commit("third", "again");
    let pack3 = work.create_incremental_pack(&c3, &c2);
    let ingested3 = ingest_pack_data(&handle, pack3).await.unwrap();
    let txn3 = make_txn(vec![("refs/heads/main", &c2, &c3)]);
    let result3 = handle
        .publish_push(Some(ingested3), txn3, HashMap::new())
        .await
        .unwrap();
    assert_eq!(result3.seq, 3);
    assert!(result3.per_ref.iter().all(|(_, r)| r.is_ok()));

    // Check manifest
    let manifest = handle.manifest();
    assert_eq!(manifest.head_seq, 3);
    assert_eq!(manifest.packs.len(), 3);

    // Check log is readable
    let log = handle.read_log(1, None).await.unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].seq, 1);
    assert_eq!(log[1].seq, 2);
    assert_eq!(log[2].seq, 3);

    // Check local refs updated
    let refs = handle.local().refs().unwrap();
    let main_ref = refs.refs.iter().find(|r| r.name == "refs/heads/main");
    assert!(main_ref.is_some());
    assert_eq!(main_ref.unwrap().oid, c3);
}

#[tokio::test]
async fn test_two_registries_cross_sync() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg_a = make_config(&cache.path().join("a"), 0);
    let cfg_b = make_config(&cache.path().join("b"), 0);
    let registry_a = Registry::new(store.clone(), Arc::new(cfg_a));
    let registry_b = Registry::new(store.clone(), Arc::new(cfg_b));

    let id = repo_id("test", "cross");

    // Create on A
    let handle_a = registry_a.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Push on A
    let work = WorkRepo::new();
    let c1 = work.commit("init", "data");
    let pack = work.create_pack();
    let ingested = ingest_pack_data(&handle_a, pack).await.unwrap();
    let txn = make_txn(vec![("refs/heads/main", "", &c1)]);
    let result = handle_a
        .publish_push(Some(ingested), txn, HashMap::new())
        .await
        .unwrap();
    assert_eq!(result.seq, 1);

    // Open on B (sync should materialize from store)
    let handle_b = registry_b.open(&id).await.unwrap();
    let _g = handle_b.sync_full().await.unwrap();

    // B should see refs + objects
    let refs = handle_b.local().refs().unwrap();
    let main_ref = refs.refs.iter().find(|r| r.name == "refs/heads/main");
    assert!(main_ref.is_some(), "B should see refs/heads/main");
    assert_eq!(main_ref.unwrap().oid, c1);

    // B should have the objects
    let oid = gix_hash::ObjectId::from_hex(c1.as_bytes()).unwrap();
    assert!(
        handle_b.local().has_object(&oid),
        "B should have the commit object"
    );
}

#[tokio::test]
async fn test_concurrent_different_refs() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 50);
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "concurrent_diff");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Create a working repo with a base commit
    let work = WorkRepo::new();
    let c0 = work.commit("base", "base");

    // Push base
    let pack0 = work.create_pack();
    let ingested0 = ingest_pack_data(&handle, pack0).await.unwrap();
    let txn0 = make_txn(vec![("refs/heads/main", "", &c0)]);
    handle
        .publish_push(Some(ingested0), txn0, HashMap::new())
        .await
        .unwrap();

    // Two concurrent pushes to different branches
    let work_a = WorkRepo::new();
    let _ca = work_a.commit("branch_a", "a");
    let ca = work_a.head();
    // Clone work_a repo and make a different commit on a different branch
    let work_b = WorkRepo::new();
    let _cb = work_b.commit("branch_b", "b");
    let cb = work_b.head();

    let pack_a = work_a.create_pack();
    let pack_b = work_b.create_pack();

    let handle_a = handle.clone();
    let handle_b = handle.clone();

    let txn_a = make_txn(vec![("refs/heads/branch_a", "", &ca)]);
    let txn_b = make_txn(vec![("refs/heads/branch_b", "", &cb)]);

    let ingested_a = ingest_pack_data(&handle, pack_a).await.unwrap();
    let ingested_b = ingest_pack_data(&handle, pack_b).await.unwrap();

    let (res_a, res_b) = tokio::join!(
        handle_a.publish_push(Some(ingested_a), txn_a, HashMap::new()),
        handle_b.publish_push(Some(ingested_b), txn_b, HashMap::new()),
    );

    let res_a = res_a.unwrap();
    let res_b = res_b.unwrap();

    // Both should succeed with distinct seqs
    assert!(
        res_a.per_ref.iter().all(|(_, r)| r.is_ok()),
        "A should succeed"
    );
    assert!(
        res_b.per_ref.iter().all(|(_, r)| r.is_ok()),
        "B should succeed"
    );
    assert_ne!(res_a.seq, res_b.seq, "Should have distinct seqs");
}

#[tokio::test]
async fn test_concurrent_same_ref_conflict() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 50);
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "concurrent_same");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Base commit
    let work = WorkRepo::new();
    let c0 = work.commit("base", "base");
    let pack0 = work.create_pack();
    let ingested0 = ingest_pack_data(&handle, pack0).await.unwrap();
    let txn0 = make_txn(vec![("refs/heads/main", "", &c0)]);
    handle
        .publish_push(Some(ingested0), txn0, HashMap::new())
        .await
        .unwrap();

    // Two concurrent pushes to the same ref with the same old value
    let work_a = WorkRepo::new();
    let _ca = work_a.commit("a_change", "aaa");
    let ca = work_a.head();
    let work_b = WorkRepo::new();
    let _cb = work_b.commit("b_change", "bbb");
    let cb = work_b.head();

    let pack_a = work_a.create_pack();
    let pack_b = work_b.create_pack();

    let ingested_a = ingest_pack_data(&handle, pack_a).await.unwrap();
    let ingested_b = ingest_pack_data(&handle, pack_b).await.unwrap();

    let handle_a = handle.clone();
    let handle_b = handle.clone();

    // Both push to refs/heads/main with old=c0
    let txn_a = make_txn(vec![("refs/heads/main", &c0, &ca)]);
    let txn_b = make_txn(vec![("refs/heads/main", &c0, &cb)]);

    let (res_a, res_b) = tokio::join!(
        handle_a.publish_push(Some(ingested_a), txn_a, HashMap::new()),
        handle_b.publish_push(Some(ingested_b), txn_b, HashMap::new()),
    );

    let res_a = res_a.unwrap();
    let res_b = res_b.unwrap();

    // One should succeed, the other should get a conflict
    let a_ok = res_a.per_ref.iter().all(|(_, r)| r.is_ok());
    let b_ok = res_b.per_ref.iter().all(|(_, r)| r.is_ok());

    assert!(a_ok || b_ok, "At least one should succeed");
    assert!(!a_ok || !b_ok, "Not both should succeed");

    // The winner should have a valid seq
    if a_ok {
        assert!(res_a.seq > 0);
    }
    if b_ok {
        assert!(res_b.seq > 0);
    }
}

#[tokio::test]
async fn test_batching_50_concurrent() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let mut cfg = make_config(cache.path(), 100); // 100ms batch window
    cfg.wal.max_batch = 64;
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "batching");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // 50 concurrent publishes to different refs.
    // Use a barrier so all tasks send their requests simultaneously,
    // ensuring they overlap within the batch window.
    let barrier = Arc::new(tokio::sync::Barrier::new(50));
    let mut handles = Vec::new();
    for i in 0..50u32 {
        let work = WorkRepo::new();
        let c = work.commit(&format!("commit_{i}"), &format!("content_{i}"));
        let pack = work.create_pack();
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        let txn = make_txn(vec![(&format!("refs/heads/branch_{i}"), "", &c)]);

        let h = handle.clone();
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            h.publish_push(Some(ingested), txn, HashMap::new()).await
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap().unwrap());
    }

    // All should succeed
    for r in &results {
        assert!(
            r.per_ref.iter().all(|(_, rr)| rr.is_ok()),
            "All pushes should succeed"
        );
        assert!(r.seq > 0);
    }

    // Manifest revision should be < 50 (batching coalesces)
    let manifest = handle.manifest();
    assert_eq!(manifest.head_seq, 50);
    assert!(
        manifest.revision < 50 + 1, // +1 for create
        "revision {} should be < 51 due to batching",
        manifest.revision
    );
    tracing::info!("batching: revision = {} for 50 pushes", manifest.revision);
}

#[tokio::test]
async fn test_checkpoint_materialize() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let mut cfg = make_config(cache.path(), 0);
    cfg.wal.snapshot_every_entries = 0; // manual checkpoint
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "checkpoint");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Push 3 entries
    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..3 {
        let c = work.commit(&format!("cp_{i}"), &format!("data_{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        let txn = make_txn(vec![("refs/heads/main", &prev, &c)]);
        handle
            .publish_push(Some(ingested), txn, HashMap::new())
            .await
            .unwrap();
        prev = c;
    }

    assert_eq!(handle.manifest().head_seq, 3);

    // Write checkpoint
    let cp_ref = handle.write_checkpoint().await.unwrap();
    assert_eq!(cp_ref.seq, 3);

    // Push one more entry (tail after checkpoint)
    let c4 = work.commit("cp_3", "data_3");
    let pack4 = work.create_incremental_pack(&c4, &prev);
    let ingested4 = ingest_pack_data(&handle, pack4).await.unwrap();
    let txn4 = make_txn(vec![("refs/heads/main", &prev, &c4)]);
    handle
        .publish_push(Some(ingested4), txn4, HashMap::new())
        .await
        .unwrap();
    assert_eq!(handle.manifest().head_seq, 4);

    // Fresh registry materializes from checkpoint + tail
    let cache2 = tempfile::tempdir().unwrap();
    let cfg2 = make_config(cache2.path(), 0);
    let registry2 = Registry::new(store.clone(), Arc::new(cfg2));
    let handle2 = registry2.open(&id).await.unwrap();
    let _g = handle2.sync_full().await.unwrap();

    // Should see the latest ref
    let refs = handle2.local().refs().unwrap();
    let main_ref = refs.refs.iter().find(|r| r.name == "refs/heads/main");
    assert!(
        main_ref.is_some(),
        "Fresh registry should see refs/heads/main"
    );
    assert_eq!(main_ref.unwrap().oid, c4);

    // Should have objects
    let oid = gix_hash::ObjectId::from_hex(c4.as_bytes()).unwrap();
    assert!(
        handle2.local().has_object(&oid),
        "Should have the latest commit"
    );
}

#[tokio::test]
async fn test_compact_replays_on_other_registry() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 0);
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "compact");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Push 3 packs
    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..3 {
        let c = work.commit(&format!("compact_{i}"), &format!("d_{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        let txn = make_txn(vec![("refs/heads/main", &prev, &c)]);
        handle
            .publish_push(Some(ingested), txn, HashMap::new())
            .await
            .unwrap();
        prev = c;
    }

    assert_eq!(handle.manifest().packs.len(), 3);

    // Repack: create a single new pack superseding all 3
    let repack_result = handle
        .local()
        .repack(gitcask_git::RepackOptions {
            mode: gitcask_git::RepackMode::Full,
            write_bitmap: false,
            write_midx: false,
            keep: vec![],
        })
        .await
        .unwrap();

    assert!(
        !repack_result.new_packs.is_empty(),
        "repack should produce new packs"
    );

    let new_pack = repack_result.new_packs[0].clone();
    let supersedes = repack_result.removed.clone();

    // Publish compact as a tier-1 pack.
    let seq = handle
        .publish_compact(new_pack.clone(), supersedes.clone(), 1)
        .await
        .unwrap();
    assert!(seq > 3);

    // Check manifest on A
    let manifest = handle.manifest();
    assert!(manifest.packs.len() < 4, "compact should reduce pack count");

    // Open on B (fresh registry)
    let cache2 = tempfile::tempdir().unwrap();
    let cfg2 = make_config(cache2.path(), 0);
    let registry2 = Registry::new(store.clone(), Arc::new(cfg2));
    let handle2 = registry2.open(&id).await.unwrap();
    let _g = handle2.sync_full().await.unwrap();

    // B should have the new pack installed and superseded removed
    let b_packs = handle2.local().packs().unwrap();
    let b_checksums: std::collections::HashSet<String> =
        b_packs.iter().map(|p| p.checksum.to_string()).collect();
    assert!(
        b_checksums.contains(&new_pack.checksum.to_string()),
        "B should have the new compacted pack"
    );
    for s in &supersedes {
        assert!(
            !b_checksums.contains(&s.to_string()),
            "B should not have superseded pack {s}"
        );
    }

    // Objects should be readable
    let oid = gix_hash::ObjectId::from_hex(prev.as_bytes()).unwrap();
    assert!(
        handle2.local().has_object(&oid),
        "B should have objects from compacted pack"
    );
}

/// A compacted pack published with a commit-graph layer: followers download
/// the side-file, install it as their chain base, and fold later pushes in as
/// incremental layers (commit-graph maintenance is async after a push).
#[tokio::test]
async fn test_commit_graph_travels_with_compacted_pack_and_grows_on_push() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 0);
    let registry = Registry::new(store.clone(), Arc::new(cfg));
    let id = repo_id("test", "cgraph");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..6 {
        let c = work.commit(&format!("base_{i}"), &format!("d_{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        handle
            .publish_push(
                Some(ingested),
                make_txn(vec![("refs/heads/main", &prev, &c)]),
                HashMap::new(),
            )
            .await
            .unwrap();
        prev = c;
    }
    let repack = handle
        .local()
        .repack(gitcask_git::RepackOptions {
            mode: gitcask_git::RepackMode::Full,
            write_bitmap: false,
            write_midx: false,
            keep: vec![],
        })
        .await
        .unwrap();
    let mut compacted = repack.new_packs[0].clone();
    handle
        .local()
        .write_pack_commit_graph(&compacted.checksum, true)
        .await
        .unwrap();
    compacted.has_commit_graph = true;
    handle
        .publish_compact(compacted.clone(), repack.removed.clone(), 1)
        .await
        .unwrap();
    let m = handle.manifest();
    let pr = m
        .packs
        .iter()
        .find(|p| p.checksum == compacted.checksum.to_string())
        .unwrap();
    assert!(
        pr.has_commit_graph,
        "manifest advertises the commit-graph side-file"
    );
    assert!(
        handle
            .store()
            .head(&gitcask_proto::keys::commit_graph_key(&pr.checksum))
            .await
            .unwrap()
            .is_some(),
        "side-file uploaded"
    );

    // Follower B: compacted layer installed by the full sync.
    let cache2 = tempfile::tempdir().unwrap();
    let registry2 = Registry::new(store.clone(), Arc::new(make_config(cache2.path(), 0)));
    let handle2 = registry2.open(&id).await.unwrap();
    {
        let _g = handle2.sync_full().await.unwrap();
    }
    let chain = handle2.local().commit_graph_chain().unwrap();
    assert_eq!(
        chain,
        handle.local().commit_graph_chain().unwrap(),
        "B installed A's compacted layer"
    );
    assert_eq!(chain.len(), 1);
    assert!(
        handle2
            .local()
            .pack_path(&compacted.checksum)
            .with_extension("commit-graph")
            .exists()
    );

    // A push through B grows B's chain by an incremental layer (async).
    let c = work.commit("after_base", "x");
    let pack = work.create_incremental_pack(&c, &prev);
    let ingested = ingest_pack_data(&handle2, pack).await.unwrap();
    handle2
        .publish_push(
            Some(ingested),
            make_txn(vec![("refs/heads/main", &prev, &c)]),
            HashMap::new(),
        )
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chain2 = handle2.local().commit_graph_chain().unwrap();
        if chain2.len() == 2 {
            assert_eq!(chain2[0], chain[0], "compacted layer untouched");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "chain did not grow: {chain2:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(handle2.local().path())
        .args(["commit-graph", "verify"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A (which published the compacted pack) sees the push on its next full sync and
    // folds it in too.
    {
        let _g = handle.sync_full().await.unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while handle.local().commit_graph_chain().unwrap().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "A's chain did not grow"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn test_orphan_log_invisible_and_cleaned() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 0);
    let registry = Registry::new(store.clone(), Arc::new(cfg));

    let id = repo_id("test", "orphan");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    // Write an orphan log object (not referenced by manifest)
    let orphan_entry = gitcask_proto::v1::LogEntry {
        seq: 99,
        kind: EntryKind::Push as i32,
        pack: None,
        txn: Some(make_txn(vec![("refs/heads/orphan", "", "deadbeef")])),
        supersedes: vec![],
        checkpoint: None,
        created_at: Some(gitcask_proto::time::now()),
        writer: "orphan_test".to_string(),
        meta: HashMap::new(),
    };
    let orphan_bytes = gitcask_proto::frame::encode_entries(std::iter::once(&orphan_entry));
    let orphan_key = gitcask_proto::keys::log_segment_key(99);
    handle
        .store()
        .put(
            &orphan_key,
            bytes::Bytes::from(orphan_bytes).into(),
            gitcask_store::PutMode::Create.into(),
        )
        .await
        .unwrap();

    // Push a real entry
    let work = WorkRepo::new();
    let c1 = work.commit("real", "data");
    let pack = work.create_pack();
    let ingested = ingest_pack_data(&handle, pack).await.unwrap();
    let txn = make_txn(vec![("refs/heads/main", "", &c1)]);
    handle
        .publish_push(Some(ingested), txn, HashMap::new())
        .await
        .unwrap();

    // B opens and syncs
    let cache2 = tempfile::tempdir().unwrap();
    let cfg2 = make_config(cache2.path(), 0);
    let registry2 = Registry::new(store.clone(), Arc::new(cfg2));
    let handle2 = registry2.open(&id).await.unwrap();
    let _g = handle2.sync_full().await.unwrap();

    // B should NOT see the orphan ref
    let refs = handle2.local().refs().unwrap();
    assert!(
        !refs.refs.iter().any(|r| r.name == "refs/heads/orphan"),
        "Orphan ref should not be visible"
    );

    // B should see only the real ref
    assert!(
        refs.refs
            .iter()
            .any(|r| r.name == "refs/heads/main" && r.oid == c1),
        "Real ref should be visible"
    );

    // Orphan log entry should not appear in read_log
    let log = handle2.read_log(1, None).await.unwrap();
    assert!(
        !log.iter().any(|e| e.seq == 99),
        "Orphan entry should not be in log"
    );
}

#[tokio::test]
async fn test_zero_oid_means_absent_for_create_and_delete() {
    // The proto documents the all-zero id as "does not exist"; the server
    // normalizes it to "" but the CLI (import) and other writers send zeros.
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let cfg = make_config(cache.path(), 0);
    let registry = Registry::new(store.clone(), Arc::new(cfg));
    let id = repo_id("test", "zero");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();

    let zero = "0".repeat(40);
    let work = WorkRepo::new();
    let c1 = work.commit("first", "hello");
    let pack = work.create_pack();
    let ingested = ingest_pack_data(&handle, pack).await.unwrap();
    let txn = make_txn(vec![
        ("refs/heads/main", &zero, &c1),
        ("refs/tags/t", &zero, &c1),
    ]);
    let r = handle
        .publish_push(Some(ingested), txn, HashMap::new())
        .await
        .unwrap();
    assert_eq!(r.seq, 1);
    assert!(r.per_ref.iter().all(|(_, r)| r.is_ok()), "{:?}", r.per_ref);

    // Creating again with zero old must conflict; deleting with zero new must work.
    let r = handle
        .publish_ref_update(make_txn(vec![("refs/tags/t", &zero, &c1)]), HashMap::new())
        .await
        .unwrap();
    assert!(r.per_ref[0].1.is_err());
    let r = handle
        .publish_ref_update(make_txn(vec![("refs/tags/t", &c1, &zero)]), HashMap::new())
        .await
        .unwrap();
    assert!(r.per_ref[0].1.is_ok(), "{:?}", r.per_ref);
    assert!(
        handle
            .local()
            .refs()
            .unwrap()
            .refs
            .iter()
            .all(|x| x.name != "refs/tags/t")
    );
}

#[tokio::test]
async fn test_checkpoint_from_refs_level_instance() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "refscp");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..3 {
        let c = work.commit(&format!("c{i}"), &format!("d{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        handle
            .publish_push(
                Some(ingested),
                make_txn(vec![("refs/heads/main", &prev, &c)]),
                HashMap::new(),
            )
            .await
            .unwrap();
        prev = c;
    }
    // Mark a pack as carrying a commit-graph so the inventory is observable.
    let repack = handle
        .local()
        .repack(gitcask_git::RepackOptions {
            mode: gitcask_git::RepackMode::Full,
            write_bitmap: true,
            write_midx: false,
            keep: vec![],
        })
        .await
        .unwrap();
    let mut compacted = repack.new_packs[0].clone();
    handle
        .local()
        .write_pack_commit_graph(&compacted.checksum, false)
        .await
        .unwrap();
    compacted.has_commit_graph = true;
    handle
        .publish_compact(compacted, repack.removed.clone(), 1)
        .await
        .unwrap();
    let head = handle.manifest().head_seq;

    // A refs-only instance checkpoints without downloading packs.
    let cache2 = tempfile::tempdir().unwrap();
    let registry2 = Registry::new(store.clone(), Arc::new(make_config(cache2.path(), 0)));
    let handle2 = registry2.open(&id).await.unwrap();
    let cp = handle2.write_checkpoint().await.unwrap();
    assert_eq!(cp.seq, head);
    assert!(cp.created_at.is_some());
    assert!(
        handle2.local().packs().unwrap().is_empty(),
        "no pack was downloaded"
    );
    let m = handle2.manifest();
    assert_eq!(m.checkpoint.as_ref().unwrap().seq, head);
    assert!(
        m.log_segments.is_empty(),
        "log folded: {:?}",
        m.log_segments
    );
    assert_eq!(handle2.checkpoint_due(), None);

    // The checkpoint object carries the pack inventory with side-file flags.
    use prost::Message;
    let (_, bytes) = gitcask_store::ObjectStoreExt::get_bytes(handle2.store(), &cp.key)
        .await
        .unwrap()
        .unwrap();
    let cpo = gitcask_proto::v1::Checkpoint::decode(bytes.as_ref()).unwrap();
    assert_eq!(cpo.packs.len(), 1);
    assert!(cpo.packs[0].has_commit_graph && cpo.packs[0].has_bitmap);
    assert_eq!(cpo.packs[0].tier, 1);
    assert_eq!(cpo.ref_count, 1);

    // Cold reader: refs from checkpoint (no log segments left), no objects needed.
    let cache3 = tempfile::tempdir().unwrap();
    let registry3 = Registry::new(store.clone(), Arc::new(make_config(cache3.path(), 0)));
    let handle3 = registry3.open(&id).await.unwrap();
    let _g = handle3.sync_refs().await.unwrap();
    let refs = handle3.local().refs().unwrap();
    assert!(
        refs.refs
            .iter()
            .any(|r| r.name == "refs/heads/main" && r.oid == prev)
    );
}

#[tokio::test]
async fn test_annotate_pack_retrofits_commit_graph() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    // The publish below folds the pack into the commit-graph chain in the background; this test
    // builds the layer by hand and would race it for `commit-graph-chain.lock` (flaky ~50 %).
    let mut cfg = make_config(cache.path(), 0);
    cfg.git.commit_graph = false;
    let registry = Registry::new(store.clone(), Arc::new(cfg));
    let id = repo_id("test", "annotate");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let c = work.commit("one", "1");
    let ingested = ingest_pack_data(&handle, work.create_pack()).await.unwrap();
    handle
        .publish_push(
            Some(ingested),
            make_txn(vec![("refs/heads/main", "", &c)]),
            HashMap::new(),
        )
        .await
        .unwrap();
    let pushed = handle.manifest().packs[0].clone();
    assert!(!pushed.has_commit_graph);
    let checksum = gix_hash::ObjectId::from_hex(pushed.checksum.as_bytes()).unwrap();
    // Build the layer after the fact and attach it.
    handle
        .local()
        .write_pack_commit_graph(&checksum, false)
        .await
        .unwrap();
    let layer = handle
        .local()
        .pack_path(&checksum)
        .with_extension("commit-graph");
    let p = handle
        .annotate_pack(&pushed.checksum, None, None, Some(layer))
        .await
        .unwrap();
    assert!(p.has_commit_graph);
    assert!(
        store
            .head(&format!(
                "{}{}",
                id.store_prefix(),
                gitcask_proto::keys::commit_graph_key(&pushed.checksum)
            ))
            .await
            .unwrap()
            .is_some()
    );
    let cache2 = tempfile::tempdir().unwrap();
    let registry2 = Registry::new(store.clone(), Arc::new(make_config(cache2.path(), 0)));
    let handle2 = registry2.open(&id).await.unwrap();
    {
        let _g = handle2.sync_full().await.unwrap();
    }
    assert_eq!(
        handle2.local().commit_graph_chain().unwrap(),
        handle.local().commit_graph_chain().unwrap()
    );
}

/// A refs-level request must never wait behind a pack materialization: the
/// pack phase runs under its own mutex, the refs phase under the sync mutex,
/// so `sync_refs()` on a cold instance answers while packs still download.
#[tokio::test]
async fn test_refs_sync_is_not_blocked_by_pack_materialization() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "unblocked");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..12 {
        let c = work.commit(&format!("c{i}"), &format!("d{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        handle
            .publish_push(
                Some(ingested),
                make_txn(vec![("refs/heads/main", &prev, &c)]),
                HashMap::new(),
            )
            .await
            .unwrap();
        prev = c;
    }
    // Slow store for the sibling: every object read costs 150 ms. 12 packs
    // now fetch pack ∥ idx (no HEAD) so wall is a few waves, not
    // 12 × 4 serial ops — still long enough that refs must not wait on it.
    let mut slow = MemoryStore::shared();
    {
        let inner = Arc::get_mut(&mut slow).unwrap();
        inner.latency = Some(Duration::from_millis(150));
    }
    // Copy the data over.
    use futures::StreamExt;
    let mut keys = store.list("", None);
    while let Some(m) = keys.next().await {
        let m = m.unwrap();
        let (_, bytes) = gitcask_store::ObjectStoreExt::get_bytes(&*store, &m.key)
            .await
            .unwrap()
            .unwrap();
        gitcask_store::ObjectStoreExt::put_bytes(
            &*slow,
            &m.key,
            bytes.to_vec(),
            gitcask_store::PutMode::Overwrite,
        )
        .await
        .unwrap();
    }
    let cache2 = tempfile::tempdir().unwrap();
    let registry2 = Registry::new(slow.clone(), Arc::new(make_config(cache2.path(), 0)));
    let handle2 = registry2.open(&id).await.unwrap();
    let h = handle2.clone();
    let packs = tokio::spawn(async move {
        let t = std::time::Instant::now();
        let _g = h.sync_full().await.unwrap();
        t.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(400)).await; // the pack phase is under way
    let t = std::time::Instant::now();
    let g = tokio::time::timeout(Duration::from_secs(5), handle2.sync_refs())
        .await
        .expect("refs sync must not wait for packs")
        .unwrap();
    let refs_ms = t.elapsed().as_millis();
    assert!(
        g.local()
            .refs()
            .unwrap()
            .refs
            .iter()
            .any(|r| r.name == "refs/heads/main")
    );
    drop(g);
    let pack_time = packs.await.unwrap();
    assert!(
        pack_time.as_millis() > 200,
        "materialization should have hit the slow store: {pack_time:?}"
    );
    assert!(
        refs_ms < 700,
        "refs sync took {refs_ms} ms while packs were materializing ({pack_time:?})"
    );
}

/// Cold opens of unrelated repositories must yield while their local git/fs
/// work runs. With two async workers, doing that work inline takes roughly
/// eight waves for sixteen repos; the blocking pool lets them overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sixteen_cold_refs_opens_are_not_thread_count_limited() {
    let cache = tempfile::tempdir().unwrap();
    let mut store = MemoryStore::shared();
    Arc::get_mut(&mut store).unwrap().latency = Some(Duration::from_millis(40));
    let mut cfg = make_config(cache.path(), 0);
    cfg.cache.evict_idle_after = Duration::ZERO;
    let registry = Registry::new(store, Arc::new(cfg));
    let ids: Vec<_> = (0..17)
        .map(|index| repo_id("cold", &format!("r{index}")))
        .collect();
    for id in &ids {
        registry.create(id, ObjectFormat::Sha1).await.unwrap();
    }
    assert_eq!(registry.evict_idle().await.unwrap().evicted, ids.len());

    let single_started = std::time::Instant::now();
    let single = registry.open(&ids[0]).await.unwrap();
    drop(single.sync_refs_only().await.unwrap());
    let single_elapsed = single_started.elapsed();

    let burst_started = std::time::Instant::now();
    let opens = ids[1..].iter().cloned().map(|id| {
        let registry = registry.clone();
        async move {
            let handle = registry.open(&id).await.unwrap();
            drop(handle.sync_refs_only().await.unwrap());
        }
    });
    futures::future::join_all(opens).await;
    let burst_elapsed = burst_started.elapsed();

    assert!(
        burst_elapsed < single_elapsed * 4,
        "16 cold refs opens took {burst_elapsed:?}, single open {single_elapsed:?}"
    );
}

#[tokio::test]
async fn test_refs_sync_never_waits_behind_a_long_read_guard() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "longread");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let mut prev = String::new();
    for i in 0..3 {
        let c = work.commit(&format!("c{i}"), &format!("d{i}"));
        let pack = if prev.is_empty() {
            work.create_pack()
        } else {
            work.create_incremental_pack(&c, &prev)
        };
        let ingested = ingest_pack_data(&handle, pack).await.unwrap();
        handle
            .publish_push(
                Some(ingested),
                make_txn(vec![("refs/heads/main", &prev, &c)]),
                HashMap::new(),
            )
            .await
            .unwrap();
        prev = c;
    }
    // Sibling B with its packs installed, then a long-lived read guard (the clone).
    let cache2 = tempfile::tempdir().unwrap();
    let registry2 = Registry::new(store.clone(), Arc::new(make_config(cache2.path(), 0)));
    let handle2 = registry2.open(&id).await.unwrap();
    let long_guard = handle2.sync_full().await.unwrap();
    // Meanwhile A compacts: B's next pack phase wants to remove 3 superseded packs.
    let repack = handle
        .local()
        .repack(gitcask_git::RepackOptions {
            mode: gitcask_git::RepackMode::Full,
            write_bitmap: false,
            write_midx: false,
            keep: vec![],
        })
        .await
        .unwrap();
    handle
        .publish_compact(repack.new_packs[0].clone(), repack.removed.clone(), 1)
        .await
        .unwrap();
    // B: a full sync (would queue the removal writer) then refs syncs: all fast.
    let h = handle2.clone();
    let full = tokio::spawn(async move {
        let g = tokio::time::timeout(Duration::from_secs(5), h.sync_full())
            .await
            .expect("full sync must not hang")
            .unwrap();
        drop(g);
    });
    for _ in 0..5 {
        let t = std::time::Instant::now();
        let g = tokio::time::timeout(Duration::from_secs(2), handle2.sync_refs())
            .await
            .expect("refs sync must not wait for the long reader")
            .unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "refs sync took {:?}",
            t.elapsed()
        );
        drop(g);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    full.await.unwrap();
    // D19 observability: whatever these refs syncs queued on (`sync_mutex`, `rw.read`), they never
    // waited behind the long reader — the recorded maximum stays far below the 1 s warn threshold.
    for (lock, max_ms) in gitcask_wal::lockwait::snapshot_for(&id) {
        if lock == "sync_mutex" || lock == "rw.read" {
            assert!(
                max_ms < 10,
                "{lock}: a refs-level request waited {max_ms} ms behind a long read guard"
            );
        }
    }
    // Superseded packs are still on disk while the long reader lives …
    let superseded = &repack.removed;
    assert!(
        superseded
            .iter()
            .any(|o| handle2.local().pack_path(o).exists()),
        "removal deferred while a reader is active"
    );
    drop(long_guard);
    // … and go on the next pack phase.
    {
        let _g = handle2.sync_full().await.unwrap();
    }
    assert!(
        superseded
            .iter()
            .all(|o| !handle2.local().pack_path(o).exists()),
        "removed once readers are gone"
    );
}

/// History replay: entries published with an explicit `created_at` carry it
/// (the WAL's time order = history), must be monotonic (an older time is
/// rejected with the reason), and non-ancestral moves of `main` are fine at
/// the WAL level (fast-forward is receive-pack/policy). `refs_as_of` then
/// answers per slot.
#[tokio::test]
async fn test_publish_at_explicit_monotonic_created_at() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "replay");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let c1 = work.commit("one", "1");
    let c2 = work.commit("two", "2");
    // Branch off c1 (non-ancestral to c2).
    std::process::Command::new("git")
        .args(["checkout", "-q", "-b", "side", &c1])
        .current_dir(work.path())
        .output()
        .unwrap();
    let c3 = work.commit("three-side", "3");
    // One pack with every commit (HEAD is on `side`; include main's c2 too).
    let pack = work.create_incremental_pack(&c3, "");
    let pack = {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "git rev-list --objects {c2} {c3} | git pack-objects --stdout"
            ))
            .current_dir(work.path())
            .output()
            .unwrap();
        let _ = pack;
        out.stdout
    };
    let ingested = ingest_pack_data(&handle, pack).await.unwrap();
    let t = |s: &str| {
        std::time::UNIX_EPOCH
            + Duration::from_secs(
                chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp() as u64
            )
    };
    // Slot 1: main = c1 at Aug 10.
    handle
        .publish_push_at(
            Some(ingested),
            make_txn(vec![("refs/heads/main", "", &c1)]),
            HashMap::new(),
            t("2026-08-10T23:00:00Z"),
        )
        .await
        .unwrap();
    // Slot 2: main = c2 at Aug 11.
    let r = handle
        .publish_push_at(
            None,
            make_txn(vec![("refs/heads/main", &c1, &c2)]),
            HashMap::new(),
            t("2026-08-11T23:00:00Z"),
        )
        .await
        .unwrap();
    assert!(r.per_ref.iter().all(|(_, r)| r.is_ok()), "{:?}", r.per_ref);
    // Older than head: rejected with the reason; nothing published.
    let r = handle
        .publish_push_at(
            None,
            make_txn(vec![("refs/heads/main", &c2, &c3)]),
            HashMap::new(),
            t("2026-08-09T00:00:00Z"),
        )
        .await
        .unwrap();
    assert!(
        r.per_ref.iter().all(
            |(_, r)| matches!(r, Err(gitcask_wal::RefError::Rejected(m)) if m.contains("monotonic"))
        ),
        "{:?}",
        r.per_ref
    );
    assert_eq!(handle.manifest().head_seq, 2);
    // Non-ancestral move (c2 -> c3) at a later time: allowed at the WAL level.
    let r = handle
        .publish_push_at(
            None,
            make_txn(vec![("refs/heads/main", &c2, &c3)]),
            HashMap::new(),
            t("2026-08-12T23:00:00Z"),
        )
        .await
        .unwrap();
    assert!(r.per_ref.iter().all(|(_, r)| r.is_ok()), "{:?}", r.per_ref);
    // Entry times are the explicit ones, in order.
    let log = handle.read_log(1, None).await.unwrap();
    let times: Vec<i64> = log
        .iter()
        .map(|e| e.created_at.as_ref().unwrap().seconds)
        .collect();
    assert_eq!(times, vec![1786402800, 1786489200, 1786575600]);
    // A plain publish (now) after explicit times is fine (now > Aug 12).
    let c4 = work.commit("four", "4");
    let ingested = ingest_pack_data(&handle, work.create_incremental_pack(&c4, &c3))
        .await
        .unwrap();
    let r = handle
        .publish_push(
            Some(ingested),
            make_txn(vec![("refs/heads/main", &c3, &c4)]),
            HashMap::new(),
        )
        .await
        .unwrap();
    assert!(r.per_ref.iter().all(|(_, r)| r.is_ok()));
}

/// The WAL commit is the truth. When the manifest CAS landed but applying the ref txn to THIS copy
/// fails (here: a stale `refs/heads/main.lock`, the lock collision the rig hit once in 2 450 rounds,
/// 2026-08-23), the pusher is still answered `ok` — the push is durable — and the copy repairs itself
/// on the next sync (the version was not advertised, so the sync replays the entry). Answering an
/// error produced a durable push that git reported as failed.
#[tokio::test]
async fn a_landed_cas_is_ok_even_when_the_local_apply_fails_and_the_next_sync_repairs_it() {
    let cache = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "lockedref");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();
    let c0 = work.commit("c0", "d0");
    let ingested = ingest_pack_data(&handle, work.create_pack()).await.unwrap();
    handle
        .publish_push(
            Some(ingested),
            make_txn(vec![("refs/heads/main", "", &c0)]),
            HashMap::new(),
        )
        .await
        .unwrap();
    drop(handle.sync_refs().await.unwrap());

    // Make the local apply fail: git refuses to take a ref lock that already exists.
    let lock = handle.local().path().join("refs/heads/main.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "stale\n").unwrap();

    let c1 = work.commit("c1", "d1");
    let ingested = ingest_pack_data(&handle, work.create_incremental_pack(&c1, &c0))
        .await
        .unwrap();
    let res = handle
        .publish_push_synced(
            Some(ingested),
            make_txn(vec![("refs/heads/main", &c0, &c1)]),
            HashMap::new(),
        )
        .await;
    assert!(
        res.is_ok(),
        "the CAS landed: the push is durable and must be acknowledged: {:?}",
        res.err().map(|e| e.to_string())
    );
    // The bucket has it …
    let (_, m) = gitcask_store::coord::get_message::<gitcask_proto::v1::Manifest>(
        handle.store(),
        gitcask_proto::keys::MANIFEST,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(m.head_seq, 2);
    // … the local copy is behind (the lock blocked update-ref) …
    assert_eq!(
        handle
            .local()
            .ref_view()
            .unwrap()
            .get("refs/heads/main")
            .unwrap_or_default(),
        c0
    );
    // … and the next sync repairs it once the lock is gone.
    std::fs::remove_file(&lock).unwrap();
    drop(handle.sync_refs().await.unwrap());
    assert_eq!(
        handle
            .local()
            .ref_view()
            .unwrap()
            .get("refs/heads/main")
            .unwrap_or_default(),
        c1
    );
    assert_eq!(handle.applied_seq(), 2);
}

/// A lost manifest-CAS response makes the publisher recover the committed version with HEAD.
/// If that HEAD also fails, durability still wins: acknowledge the push, invalidate the cached
/// version, and let the next sync replay the committed log entry.
#[tokio::test]
async fn a_landed_cas_is_ok_even_when_version_recovery_fails() {
    let cache = tempfile::tempdir().unwrap();
    let truth = MemoryStore::shared();
    let store = FaultStore::new(truth, "version-recovery", 17);
    let registry = Registry::new(store.clone(), Arc::new(make_config(cache.path(), 0)));
    let id = repo_id("test", "version-recovery");
    let handle = registry.create(&id, ObjectFormat::Sha1).await.unwrap();
    let work = WorkRepo::new();

    let c0 = work.commit("c0", "d0");
    let ingested = ingest_pack_data(&handle, work.create_pack()).await.unwrap();
    handle
        .publish_push(
            Some(ingested),
            make_txn(vec![("refs/heads/main", "", &c0)]),
            HashMap::new(),
        )
        .await
        .unwrap();
    drop(handle.sync_full().await.unwrap());

    let c1 = work.commit("c1", "d1");
    let ingested = ingest_pack_data(&handle, work.create_incremental_pack(&c1, &c0))
        .await
        .unwrap();
    store.set(
        FaultPlan {
            // The manifest CAS lands but its response is lost, forcing version recovery.
            p_err_after: 1.0,
            // The immediately following HEAD fails exactly once.
            fail_once_keys: vec!["head:manifest.pb".into()],
            ..Default::default()
        }
        .with_only(&["manifest.pb"]),
    );

    let result = handle
        .publish_push_synced(
            Some(ingested),
            make_txn(vec![("refs/heads/main", &c0, &c1)]),
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "the manifest CAS landed, so recovery metadata failures must not reject the push: {:?}",
        result.err().map(|error| error.to_string())
    );
    assert!(
        handle.manifest_version().is_none(),
        "failed version recovery must force an unconditional manifest refetch"
    );

    drop(handle.sync_full().await.unwrap());
    assert_eq!(
        handle
            .local()
            .ref_view()
            .unwrap()
            .get("refs/heads/main")
            .unwrap_or_default(),
        c1
    );
    assert_eq!(handle.applied_seq(), 2);
}
