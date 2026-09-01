//! Synchronization: freshness check, catch-up, materialization.

use std::sync::Arc;

use crate::error::WalError;
use gitcask_git::LocalRepo;
use gitcask_proto::keys;
use gitcask_proto::v1::{EntryKind, LogEntry, Manifest, PackRef, RefSnapshot};
use gitcask_store::coord::{get_message, get_message_if_changed};
use gitcask_store::{GetOptions, GetResult, ObjectStore, Prefixed, Version};
use tracing::Instrument;

/// A read guard held for the lifetime of a request. While any guard is alive
/// no pack is removed locally (the inner RwLock read guard prevents it).
pub struct ReadGuard<'a> {
    pub(crate) _guard: tokio::sync::RwLockReadGuard<'a, ()>,
    pub(crate) handle: &'a super::handle::RepoHandle,
}

impl<'a> ReadGuard<'a> {
    pub fn manifest(&self) -> Arc<Manifest> {
        self.handle.manifest.read().clone()
    }
    pub fn local(&self) -> &LocalRepo {
        &self.handle.local
    }
}

/// How much of the WAL a sync must bring to the local copy.
///
/// `Refs` applies the checkpoint ref snapshot and every log entry's ref
/// transaction but downloads no packs: enough for `info/refs`, `ls-refs`,
/// and the web `refs` endpoint, i.e. everything a cold instance
/// must answer instantly. `Full` additionally reconciles the local pack set
/// with `Manifest.packs` (download missing, drop superseded) and is required
/// before serving or verifying objects (upload-pack, receive-pack, compaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncLevel {
    Refs,
    Full,
}

impl SyncLevel {
    /// Whether this level reconciles the local pack set.
    pub fn wants_packs(self) -> bool {
        self == SyncLevel::Full
    }
}

/// Result of a sync operation, holding either a read guard (common case) or
/// indicating the repo was not found.
pub(crate) enum SyncOutcome {
    Unchanged,
    Changed {
        meta_version: Version,
        manifest: Manifest,
    },
}

/// Perform a conditional GET on manifest.pb and return the outcome.
pub(crate) async fn freshness_check(
    store: &Prefixed,
    known: &Option<Version>,
) -> Result<SyncOutcome, WalError> {
    match known {
        Some(v) => match get_message_if_changed::<Manifest>(store, keys::MANIFEST, v).await? {
            None => Ok(SyncOutcome::Unchanged),
            Some((meta, manifest)) => Ok(SyncOutcome::Changed {
                meta_version: meta.version,
                manifest,
            }),
        },
        None => match get_message::<Manifest>(store, keys::MANIFEST).await? {
            None => Err(WalError::NotFound),
            Some((meta, manifest)) => Ok(SyncOutcome::Changed {
                meta_version: meta.version,
                manifest,
            }),
        },
    }
}

/// Download a pack+idx from the store and install it into the local repo.
/// Uses a temp file, then `install_pack` (atomic rename).
pub(crate) async fn download_and_install_pack(
    store: &Prefixed,
    local: &LocalRepo,
    pack: &PackRef,
    tmp_dir: &std::path::Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<(), WalError> {
    let span = tracing::info_span!(
        "wal.download_pack",
        checksum = %pack.checksum,
        bytes = pack.pack_size,
        objects = pack.object_count,
    );
    // Instrument awaited futures instead of carrying a thread-local span guard
    // across await points.

    let checksum = &pack.checksum;
    let pack_key = keys::pack_key(checksum);
    let idx_key = keys::idx_key(checksum);

    // Skip if already installed locally.
    let oid = gix_hash::ObjectId::from_hex(checksum.as_bytes())
        .map_err(|e| WalError::Corrupt(format!("invalid pack checksum {checksum}: {e}")))?;
    if tokio::fs::try_exists(local.pack_path(&oid)).await? {
        // Already installed; only fetch side files the manifest now advertises.
        for (flag, ext, key) in side_files(pack) {
            if !flag {
                continue;
            }
            let dest = local.pack_path(&oid).with_extension(ext);
            if tokio::fs::try_exists(&dest).await? {
                continue;
            }
            let tmp = tmp_dir.join(format!("{checksum}.{ext}"));
            if let Err(e) = download_object(store, &key, &tmp, None, None)
                .instrument(span.clone())
                .await
            {
                tracing::warn!(checksum = %checksum, ext, error = %e, "side file download failed");
                continue;
            }
            if tokio::fs::rename(&tmp, &dest).await.is_err() {
                let _ = tokio::fs::copy(&tmp, &dest).await;
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        }
        return Ok(());
    }

    let pack_path = tmp_dir.join(format!("pack-{checksum}.pack"));
    let idx_path = tmp_dir.join(format!("pack-{checksum}.idx"));

    // Pack + idx + advertised side-files in one round: they are independent
    // immutable objects. Sequential was pack then idx then each side-file
    // (measurements: a 2.1 GB idx at ~10 MB/s *then* the 0.4 GB bitmap at
    // 40 MB/s). Each download is already striped.
    let mut extra = Vec::new();
    let mut side_futs = Vec::new();
    for (flag, ext, key) in side_files(pack) {
        if !flag {
            continue;
        }
        let path = tmp_dir.join(format!("pack-{checksum}.{ext}"));
        extra.push(path.clone());
        side_futs.push(
            async move { download_object(store, &key, &path, None, None).await }
                .instrument(span.clone()),
        );
    }
    let (pack_r, idx_r, side_rs) = tokio::join!(
        download_object(
            store,
            &pack_key,
            &pack_path,
            nonzero(pack.pack_size),
            progress
        )
        .instrument(span.clone()),
        download_object(store, &idx_key, &idx_path, nonzero(pack.idx_size), progress)
            .instrument(span.clone()),
        futures::future::join_all(side_futs),
    );
    pack_r?;
    idx_r?;
    for r in side_rs {
        r?;
    }

    local
        .install_pack(&pack_path, &idx_path, &extra)
        .instrument(span.clone())
        .await?;
    Ok(())
}

/// `(advertised, extension, store key)` for every side-file a pack may carry.
fn side_files(pack: &PackRef) -> [(bool, &'static str, String); 3] {
    let c = &pack.checksum;
    [
        (pack.has_rev, "rev", keys::rev_key(c)),
        (pack.has_bitmap, "bitmap", keys::bitmap_key(c)),
        (
            pack.has_commit_graph,
            "commit-graph",
            keys::commit_graph_key(c),
        ),
    ]
}

/// Download an object to `dest`. Small objects stream straight to the file;
/// large ones (packs) are fetched as concurrent range reads written at their
/// offsets (object stores deliver ~100 MB/s per connection; striping gets the
/// NIC's worth), with bounded memory (PAR * CHUNK).
/// `progress(delta_bytes, total_bytes)` is called as chunks land (callers
/// throttle). `known_size` skips the happy-path HEAD (ROUNDTRIPS: HEAD ≈ GET;
/// PackRef already carries pack/idx sizes).
pub(crate) type ProgressFn<'a> = &'a (dyn Fn(u64, u64) + Send + Sync);

fn nonzero(n: u64) -> Option<u64> {
    (n > 0).then_some(n)
}

pub(crate) async fn download_object(
    store: &Prefixed,
    key: &str,
    dest: &std::path::Path,
    known_size: Option<u64>,
    progress: Option<ProgressFn<'_>>,
) -> Result<(), WalError> {
    use futures::{StreamExt, TryStreamExt};
    use std::os::unix::fs::FileExt;
    const CHUNK: u64 = 32 * 1024 * 1024;
    // 16 stripes in flight: one gRPC stream tops out around 10–20 MB/s from
    // a serverless host, the NIC well beyond 100 MB/s (a large repository's 2.1 GB idx took 217 s
    // on the broker with 8).
    const PAR: usize = 16;

    let size = match known_size {
        Some(n) => n,
        None => match store.head(key).await? {
            Some(m) => m.size,
            None => {
                return Err(WalError::Store(gitcask_store::StoreError::NotFound {
                    key: key.to_string(),
                }));
            }
        },
    };
    let report = |n: u64| {
        if let Some(p) = progress {
            p(n, size);
        }
    };
    if size <= CHUNK {
        let res = store.get(key, GetOptions::default()).await?;
        return match res {
            GetResult::Object { body, .. } => {
                let mut file = tokio::fs::File::create(dest).await?;
                let mut body = body;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk?;
                    tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                    report(chunk.len() as u64);
                }
                tokio::io::AsyncWriteExt::flush(&mut file).await?;
                Ok(())
            }
            GetResult::NotModified { .. } => {
                Err(WalError::Corrupt(format!("unexpected 304 for {key}")))
            }
        };
    }
    let dest = dest.to_path_buf();
    let file = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::create(dest)?;
        file.set_len(size)?;
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(|error| WalError::Corrupt(format!("download file task failed: {error}")))??;
    let file = std::sync::Arc::new(file);
    let starts: Vec<u64> = (0..size).step_by(CHUNK as usize).collect();
    let report = &report;
    futures::stream::iter(starts)
        .map(|start| {
            let file = file.clone();
            let end = (start + CHUNK).min(size);
            async move {
                let res = store
                    .get(
                        key,
                        GetOptions {
                            range: Some(start..end),
                            ..Default::default()
                        },
                    )
                    .await?;
                let body = match res {
                    GetResult::Object { body, .. } => body,
                    GetResult::NotModified { .. } => {
                        return Err(WalError::Corrupt(format!("unexpected 304 for {key}")));
                    }
                };
                let bytes = gitcask_store::util::collect(body, (end - start) as usize).await?;
                if bytes.len() as u64 != end - start {
                    return Err(WalError::Corrupt(format!(
                        "short range read for {key}: {}..{} got {}",
                        start,
                        end,
                        bytes.len()
                    )));
                }
                let n = bytes.len() as u64;
                tokio::task::spawn_blocking(move || file.write_all_at(&bytes, start))
                    .await
                    .map_err(|e| WalError::Corrupt(format!("write task failed: {e}")))??;
                report(n);
                Ok::<(), WalError>(())
            }
        })
        .buffer_unordered(PAR)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

/// Apply the delta between the current local state and the new manifest.
/// Downloads missing packs, replays log entries, applies ref transactions.
pub(crate) async fn apply_delta(
    handle: &super::handle::RepoHandle,
    new_manifest: &Manifest,
    new_version: &Version,
) -> Result<(), WalError> {
    let store = &handle.store;
    let local = &handle.local;
    let current_state = handle.state.lock().clone();

    // If we have a checkpoint and haven't loaded it yet, load its refs. Its
    // packs are a subset of `Manifest.packs` and are reconciled below.
    let checkpoint_seq = new_manifest.checkpoint.as_ref().map(|c| c.seq).unwrap_or(0);
    let need_checkpoint_load = checkpoint_seq > 0 && current_state.applied_seq < checkpoint_seq;

    if need_checkpoint_load {
        let refs_key = keys::checkpoint_refs_key(checkpoint_seq);
        if let Some((_, snap)) = get_message::<RefSnapshot>(store, &refs_key).await? {
            let local = local.clone();
            tokio::task::spawn_blocking(move || local.load_ref_snapshot(&snap))
                .await
                .map_err(|error| {
                    WalError::Corrupt(format!("checkpoint refs task failed: {error}"))
                })??;
            handle.state.lock().applied_seq = checkpoint_seq;
        }
    }

    // Replay log entries (refs, and superseded-pack bookkeeping) from
    // applied_seq+1 to head_seq. Packs are never touched here.
    let applied_seq = handle.state.lock().applied_seq;
    let head_seq = new_manifest.head_seq;
    if applied_seq < head_seq {
        replay_log(handle, new_manifest, applied_seq, head_seq).await?;
    }

    {
        let mut state = handle.state.lock();
        state.manifest_version = Some(new_version.as_str().to_string());
        state.applied_seq = head_seq;
        state.revision = new_manifest.revision;
    }
    let state_started = std::time::Instant::now();
    let state_path = local.path().to_path_buf();
    let state = handle.state.lock().clone();
    tokio::task::spawn_blocking(move || crate::state::save_state(&state_path, &state))
        .await
        .map_err(|error| WalError::Corrupt(format!("state save task failed: {error}")))??;
    let state_save_ms = u64::try_from(state_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let refresh_started = std::time::Instant::now();
    local.refresh_async().await?;
    tracing::info!(
        repo = %handle.id,
        state_save_ms,
        refresh_ms = u64::try_from(refresh_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "refs delta installed"
    );
    Ok(())
}

/// Make the local pack set match `manifest.packs`: download what is missing
/// (bounded concurrency, striped range reads) and remove packs superseded by
/// COMPACT entries applied since the last full sync. Idempotent; records
/// `packs_revision` on success so the next full sync is a no-op check.
pub(crate) async fn reconcile_packs(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
) -> Result<(), WalError> {
    let store = &handle.store;
    let local = &handle.local;
    // Test hook: simulate an unknown blocking call inside the install path.
    // It belongs on the blocking pool too: even the isolated bulk runtime's
    // two async workers must remain available to poll unrelated downloads.
    if let Some(ms) = std::env::var("GITCASK_TEST_BLOCK_INSTALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        })
        .await
        .map_err(|error| WalError::Corrupt(format!("install test task failed: {error}")))?;
    }
    let span = tracing::info_span!(
        "wal.reconcile_packs",
        repo = %handle.id,
        live = manifest.packs.len(),
        downloaded = 0usize,
        removed = 0usize,
    );

    let scan_local = local.clone();
    let installed_packs: std::collections::HashSet<String> =
        tokio::task::spawn_blocking(move || scan_local.packs())
            .await
            .map_err(|error| WalError::Corrupt(format!("pack scan task failed: {error}")))??
            .into_iter()
            .map(|p| p.checksum.to_string())
            .collect();
    let tmp_dir = local.path().join(".gitcask-tmp");
    tokio::fs::create_dir_all(&tmp_dir).await.ok();

    // Installed packs whose manifest entry now advertises a side-file this
    // host lacks (`annotate-pack` / the rev-index unit retrofitting a `.rev`
    // or a bitmap onto a published pack): fetch just the side-file. Without
    // this the fleet never converged after a pack gained a `.rev` in the
    // bucket and every host kept rebuilding the reverse index per fetch.
    for p in manifest
        .packs
        .iter()
        .filter(|p| installed_packs.contains(&p.checksum))
    {
        let Ok(oid) = gix_hash::ObjectId::from_hex(p.checksum.as_bytes()) else {
            continue;
        };
        for (flag, ext, key) in side_files(p) {
            let dest = local.pack_path(&oid).with_extension(ext);
            if !flag || tokio::fs::try_exists(&dest).await? {
                continue;
            }
            let tmp = tmp_dir.join(format!("pack-{}.{ext}", p.checksum));
            match download_object(store, &key, &tmp, None, None)
                .instrument(span.clone())
                .await
            {
                Ok(()) => {
                    if tokio::fs::rename(&tmp, &dest).await.is_err() {
                        let _ = tokio::fs::copy(&tmp, &dest).await;
                        let _ = tokio::fs::remove_file(&tmp).await;
                    }
                    tracing::info!(repo = %handle.id, pack = %p.checksum, ext, "side-file installed for an installed pack");
                }
                Err(e) => {
                    tracing::warn!(repo = %handle.id, pack = %p.checksum, ext, error = %e, "side-file download failed")
                }
            }
        }
    }
    let missing_packs: Vec<&PackRef> = manifest
        .packs
        .iter()
        .filter(|p| !installed_packs.contains(&p.checksum))
        .collect();
    span.record("downloaded", missing_packs.len());
    let downloaded: Vec<PackRef> = missing_packs.iter().map(|p| (*p).clone()).collect();

    let reporter = handle.reporter();
    let total_bytes: u64 = missing_packs.iter().map(|p| p.pack_size + p.idx_size).sum();
    if !missing_packs.is_empty() {
        reporter.notice(format!(
            "Materializing {} pack(s) ({} bytes) from the WAL onto this instance",
            missing_packs.len(),
            total_bytes
        ));
    }
    let done_all = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let throttle = Arc::new(crate::progress::Throttle::new(
        std::time::Duration::from_millis(250),
    ));
    // `cache.bulk_threads` controls async polling, not pack fan-out: keep at
    // most eight repositories' packs downloading/installing at once so a
    // larger runtime cannot multiply disk and memory use without bound.
    let sem = Arc::new(tokio::sync::Semaphore::new(8));
    let mut tasks = Vec::new();
    for p in missing_packs {
        let sem = sem.clone();
        let store = store.clone();
        let local = local.clone();
        let p = p.clone();
        let tmp_dir = tmp_dir.clone();
        let reporter = reporter.clone();
        let done_all = done_all.clone();
        let throttle = throttle.clone();
        tasks.push(tokio::spawn(
            async move {
                let _permit = sem.acquire().await.unwrap();
                // Per-object progress arrives as absolute (done,total); turn it
                // into deltas for the shared counter.
                let cb = |delta: u64, _t: u64| {
                    // download_object reports a per-chunk delta so pack+idx+
                    // side-files can run in one round without a shared cursor.
                    let all =
                        done_all.fetch_add(delta, std::sync::atomic::Ordering::Relaxed) + delta;
                    if throttle.tick(false) {
                        reporter.bar("Downloading packs", all, Some(total_bytes), "bytes");
                    }
                };
                download_and_install_pack(&store, &local, &p, &tmp_dir, Some(&cb)).await
            }
            .instrument(span.clone()),
        ));
    }
    let had_downloads = !tasks.is_empty();
    for t in tasks {
        t.await.map_err(|e| WalError::Corrupt(e.to_string()))??;
    }
    if had_downloads {
        reporter.bar("Downloading packs", total_bytes, Some(total_bytes), "bytes");
        reporter.notice("Packs installed; local copy is complete");
    }
    maintain_commit_graph(handle, manifest, &downloaded).await;

    // Remove packs superseded by compactions (never a pack that is still
    // live: a later PUSH/COMPACT may have re-listed it). Removal needs no
    // active readers (they hold `rw.read()` through their ReadGuard), so it
    // **tries** the write lock: if any reader is active (a clone streaming
    // for minutes), the removal simply waits for a later pass — never queue
    // as a writer, which would block every new reader on the instance.
    let live: std::collections::HashSet<&str> =
        manifest.packs.iter().map(|p| p.checksum.as_str()).collect();
    let pending = std::mem::take(&mut handle.state.lock().pending_pack_removals);
    let mut removed = 0usize;
    let mut still_pending: Vec<String> = Vec::new();
    let to_remove: Vec<(String, gix_hash::ObjectId)> = pending
        .iter()
        .filter(|s| !live.contains(s.as_str()))
        .map(|s| {
            gix_hash::ObjectId::from_hex(s.as_bytes())
                .map(|o| (s.clone(), o))
                .map_err(|e| WalError::Corrupt(format!("invalid supersedes checksum: {e}")))
        })
        .collect::<Result<_, _>>()?;
    if !to_remove.is_empty() {
        match handle.rw.try_write() {
            Ok(_w) => {
                let remove_local = local.clone();
                removed = tokio::task::spawn_blocking(move || {
                    let mut removed = 0usize;
                    for (_, oid) in &to_remove {
                        if remove_local.pack_path(oid).exists() {
                            remove_local.remove_pack(oid)?;
                            removed += 1;
                        }
                    }
                    Ok::<_, gitcask_git::GitError>(removed)
                })
                .await
                .map_err(|error| {
                    WalError::Corrupt(format!("pack removal task failed: {error}"))
                })??;
            }
            Err(_) => {
                tracing::info!(repo = %handle.id, packs = to_remove.len(), "superseded packs kept for now: readers active; retried on the next sync");
                still_pending.extend(to_remove.iter().map(|(s, _)| s.clone()));
            }
        }
    }
    span.record("removed", removed);
    if !still_pending.is_empty() {
        handle.state.lock().pending_pack_removals = still_pending;
    }

    {
        let mut state = handle.state.lock();
        state.packs_revision = manifest.revision;
    }
    let state_path = local.path().to_path_buf();
    let state = handle.state.lock().clone();
    tokio::task::spawn_blocking(move || crate::state::save_state(&state_path, &state))
        .await
        .map_err(|error| WalError::Corrupt(format!("pack state save task failed: {error}")))??;
    Ok(())
}

/// Keep the local commit-graph chain current after packs were installed:
/// a pack that ships a commit-graph layer becomes the chain base (replacing
/// whatever was there), and every other newly installed pack's
/// commits are folded in as an incremental layer (`--split --stdin-packs`,
/// cheap: generation numbers come from the existing layers). Best effort:
/// the graph is an accelerator, a failure only costs speed.
pub(crate) async fn maintain_commit_graph(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    installed: &[PackRef],
) {
    if !handle.cfg.git.commit_graph {
        return;
    }
    let local = &handle.local;
    let mut base_changed = false;
    for p in installed.iter().filter(|p| p.has_commit_graph) {
        if let Ok(oid) = gix_hash::ObjectId::from_hex(p.checksum.as_bytes()) {
            // Filesystem + gix reopen: off the runtime.
            let l = local.clone();
            let res = tokio::task::spawn_blocking(move || l.install_commit_graph_base(&oid)).await;
            match res {
                Ok(Ok(true)) => base_changed = true,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(pack = %p.checksum, error = %e, "commit-graph base install failed")
                }
                Err(e) => {
                    tracing::warn!(pack = %p.checksum, error = %e, "commit-graph base install task failed")
                }
            }
        }
    }
    // After a base change every pack without its own layer must be re-added
    // (the old chain layers were dropped); otherwise only what was installed.
    let candidates: Vec<&PackRef> = if base_changed {
        manifest
            .packs
            .iter()
            .filter(|p| !p.has_commit_graph)
            .collect()
    } else {
        installed.iter().filter(|p| !p.has_commit_graph).collect()
    };
    let candidates: Vec<gix_hash::ObjectId> = candidates
        .iter()
        .filter_map(|p| gix_hash::ObjectId::from_hex(p.checksum.as_bytes()).ok())
        .collect();
    let scan_local = local.clone();
    let packs = match tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .filter(|oid| scan_local.pack_path(oid).exists())
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(packs) => packs,
        Err(error) => {
            tracing::warn!(repo = %handle.id, %error, "commit-graph pack scan task failed");
            return;
        }
    };
    if packs.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    if let Err(e) = local
        .update_commit_graph(&packs, handle.cfg.git.commit_graph_changed_paths)
        .await
    {
        tracing::warn!(repo = %handle.id, error = %e, "commit-graph update failed");
    } else {
        tracing::info!(repo = %handle.id, packs = packs.len(), ms = started.elapsed().as_millis() as u64, "commit-graph updated");
    }
}

/// Replay log entries in (from_seq, to_seq] from the manifest's log segments.
pub(crate) async fn replay_log(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    from_seq: u64,
    to_seq: u64,
) -> Result<(), WalError> {
    let store = &handle.store;
    let store_started = std::time::Instant::now();

    // Find segments that overlap (from_seq, to_seq]
    let segments: Vec<&gitcask_proto::v1::LogSegmentRef> = manifest
        .log_segments
        .iter()
        .filter(|s| s.last_seq > from_seq && s.first_seq <= to_seq)
        .collect();

    // Fetch every segment in parallel (order kept), then apply ALL entries in
    // one pass: `apply_ref_txns_offline` rewrites packed-refs, which is O(refs)
    // — per segment that was k × 500 ms on a 500 k-ref repo (2026-08-21,
    // test/refs500k: 2 tail entries = 1 s of every cold refs sync). One
    // rewrite per sync, and the tail's GETs overlap instead of serializing.
    let keys: Vec<String> = segments.iter().map(|s| s.key.clone()).collect();
    let mut fetched: Vec<Option<bytes::Bytes>> = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(16) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|key| {
                let store = store.clone();
                let key = key.clone();
                async move {
                    let res = store.get(&key, GetOptions::default()).await?;
                    Ok::<Option<bytes::Bytes>, WalError>(match res {
                        GetResult::Object { meta, body } => {
                            Some(gitcask_store::util::collect(body, meta.size as usize).await?)
                        }
                        GetResult::NotModified { .. } => None,
                    })
                }
            })
            .collect();
        for r in futures::future::join_all(futs).await {
            fetched.push(r?);
        }
    }

    let mut all: Vec<LogEntry> = Vec::new();
    for bytes in fetched {
        let Some(bytes) = bytes else { continue };
        // Decode frames (tolerate partial trailing frame)
        let (entries, _) = gitcask_proto::frame::decode_entries(&bytes)
            .map_err(|e| WalError::Corrupt(format!("log segment decode: {e}")))?;
        all.extend(
            entries
                .into_iter()
                .filter(|e| e.seq > from_seq && e.seq <= to_seq),
        );
    }
    all.sort_by_key(|e| e.seq);
    let store_get_ms = u64::try_from(store_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(last) = all
        .last()
        .and_then(|e| e.created_at.as_ref())
        .map(gitcask_proto::time::to_system)
    {
        let mut slot = handle.last_entry_time.lock();
        if slot.map(|t| last > t).unwrap_or(true) {
            *slot = Some(last);
        }
    }
    let wanted: Vec<&LogEntry> = all.iter().collect();
    let refs_started = std::time::Instant::now();
    apply_entries(handle, &wanted).await?;
    tracing::info!(
        repo = %handle.id,
        store_get_ms,
        packed_refs_ms = u64::try_from(refs_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "WAL refs replayed"
    );

    Ok(())
}

/// Apply a batch of log entries to the local repo (refs level).
///
/// Ref transactions are merged and written once (`apply_ref_txns_offline`:
/// no `git update-ref`, so it works before the packs exist locally).
/// COMPACT entries only record their superseded packs; `reconcile_packs`
/// installs/removes packs against `Manifest.packs` on the next full sync.
pub(crate) async fn apply_entries(
    handle: &super::handle::RepoHandle,
    entries: &[&LogEntry],
) -> Result<(), WalError> {
    let mut txns: Vec<gitcask_proto::v1::RefTransaction> = Vec::new();
    let mut supersedes: Vec<String> = Vec::new();
    for entry in entries {
        match entry.kind() {
            EntryKind::Push | EntryKind::RefUpdate => {
                if let Some(txn) = &entry.txn {
                    txns.push(txn.clone());
                }
            }
            EntryKind::Compact => {
                supersedes.extend(entry.supersedes.iter().cloned());
            }
            EntryKind::Checkpoint => {}
            EntryKind::Unspecified => {
                tracing::warn!(seq = entry.seq, "unspecified log entry kind, skipping");
            }
        }
    }
    if !txns.is_empty() {
        let local = handle.local.clone();
        tokio::task::spawn_blocking(move || {
            let txns: Vec<_> = txns.iter().collect();
            local.apply_ref_txns_offline(&txns)
        })
        .await
        .map_err(|error| WalError::Corrupt(format!("packed-refs task failed: {error}")))??;
    }
    if !supersedes.is_empty() {
        let mut state = handle.state.lock();
        for s in supersedes {
            if !state.pending_pack_removals.contains(&s) {
                state.pending_pack_removals.push(s);
            }
        }
    }
    Ok(())
}

pub(crate) async fn materialize_from_scratch(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    version: &Version,
) -> Result<(), WalError> {
    let span = tracing::info_span!(
        "wal.materialize",
        repo = %handle.id,
        head_seq = manifest.head_seq,
    );
    // Instrument the awaited materialization future; do not hold an enter guard.

    // Reset state
    handle.state.lock().applied_seq = 0;

    // Apply delta from scratch (checkpoint + all log entries), then packs.
    apply_delta(handle, manifest, version)
        .instrument(span.clone())
        .await?;
    reconcile_packs(handle, manifest)
        .instrument(span.clone())
        .await
}

#[cfg(test)]
mod download_tests {
    use super::download_object;
    use gitcask_store::{ObjectStoreExt, Prefixed, PutMode, memory::MemoryStore};

    #[tokio::test]
    async fn striped_download_matches_source() {
        // > CHUNK (32 MiB) so the ranged/striped path runs, with a ragged tail.
        let size = 70 * 1024 * 1024 + 12345;
        let mut data = vec![0u8; size];
        let mut x: u64 = 0x9E3779B97F4A7C15;
        for b in data.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let store = MemoryStore::shared();
        store
            .put_bytes("p/big.pack", data.clone(), PutMode::Create)
            .await
            .unwrap();
        store
            .put_bytes("p/small.pack", b"tiny".to_vec(), PutMode::Create)
            .await
            .unwrap();
        let prefixed = Prefixed::new(store, "p/");
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.pack");
        download_object(&prefixed, "big.pack", &big, Some(size as u64), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&big).unwrap(), data);
        let small = dir.path().join("small.pack");
        download_object(&prefixed, "small.pack", &small, None, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&small).unwrap(), b"tiny");
    }
}

/// The **bulk runtime**: a small dedicated tokio runtime (own worker threads)
/// that runs pack materialization (striped downloads, 32 MiB chunk copies,
/// tmpfs writes, install renames, gix reopen, commit-graph/midx subprocess
/// waits). Whatever inside that path is CPU-heavy or secretly blocking can
/// only delay other bulk work — request workers on the main runtime keep
/// serving refs in milliseconds (prod 2026-08-20: the main runtime stalled
/// 2.6–43 s repeatedly for the whole duration of one repo's 7.5 GB + another's
/// 12 GB materializations; the watchdog caught it, the cause hid among a dozen
/// candidates; isolation makes the question moot).
static BULK_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn bulk_runtime(worker_threads: usize) -> &'static tokio::runtime::Runtime {
    BULK_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name("gitcask-bulk")
            .enable_all()
            .build()
            .expect("bulk runtime")
    })
}

/// Run `fut` on the bulk runtime and await its result from the caller's
/// runtime. The future must be `'static + Send` (use `Arc<RepoHandle>`).
pub(crate) async fn on_bulk_runtime<T: Send + 'static>(
    worker_threads: usize,
    fut: impl std::future::Future<Output = Result<T, WalError>> + Send + 'static,
) -> Result<T, WalError> {
    let span = tracing::Span::current();
    let (tx, rx) = tokio::sync::oneshot::channel();
    bulk_runtime(worker_threads).spawn(async move {
        let r = fut.instrument(span).await;
        let _ = tx.send(r);
    });
    rx.await
        .map_err(|_| WalError::Corrupt("bulk runtime task dropped".into()))?
}
