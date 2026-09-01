//! RepoHandle: per-repository state, sync, publish, checkpoint.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use gitcask_git::{LocalRepo, RepoId};
use gitcask_proto::v1::Manifest;
use gitcask_store::{Prefixed, Version};
use parking_lot::{Mutex as PLMutex, RwLock as PLRwLock};
use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock, mpsc};
use tracing::Instrument;

use crate::error::WalError;
use crate::progress::{ProgressRx, ProgressTx, Reporter};
use crate::publish::{PublishRequest, PublishResult};
use crate::state::RepoState;
use crate::sync::SyncLevel;
use crate::tasks::{Begin, Tasks};

pub(crate) fn instance_id() -> String {
    gitcask_store::coord::instance_id().to_string()
}

pub struct RepoHandle {
    pub(crate) id: RepoId,
    pub(crate) local: LocalRepo,
    pub(crate) store: Prefixed,
    pub(crate) cfg: Arc<gitcask_config::Config>,

    // Prevents pack removal during reads. Uses tokio::sync::RwLock because
    // guards are held across .await points (freshness check, apply_delta).
    pub(crate) rw: TokioRwLock<()>,
    // Single in-flight sync.
    pub(crate) sync_mutex: TokioMutex<()>,
    /// Serializes pack reconciliation (downloads/links/removals). Held
    /// *without* `sync_mutex`/`rw.write`, so a refs-level request is never
    /// stuck behind a multi-GB materialization (only removals take the write
    /// lock, briefly).
    pub(crate) pack_mutex: Arc<TokioMutex<()>>,

    // Current manifest (last known). Short critical sections, no await.
    pub(crate) manifest: PLRwLock<Arc<Manifest>>,
    pub(crate) manifest_version: PLMutex<Option<Version>>,

    // Persistent local state.
    pub(crate) state: PLMutex<RepoState>,

    // Freshness TTL.
    pub(crate) last_freshness: PLMutex<Option<Instant>>,

    // Eviction tracking.
    pub(crate) last_access: PLMutex<Instant>,

    // Self-referential Arc for spawning the publisher task.
    pub(crate) self_arc: std::sync::OnceLock<Arc<RepoHandle>>,
    // Single-flight publisher channel.
    pub(crate) publish_tx: PLMutex<Option<mpsc::UnboundedSender<PublishRequest>>>,
    // Number of callers currently waiting for a publish response. Used by
    // the publisher to distinguish a lone push from a concurrent batch.
    pub(crate) publish_waiters: AtomicUsize,
    // Single-flight background pack prefetch after a refs-level sync.
    pub(crate) prefetch_inflight: std::sync::atomic::AtomicBool,
    /// Time of the newest log entry seen (replayed or published): the floor
    /// for explicit `created_at` on publish.
    pub(crate) last_entry_time: parking_lot::Mutex<Option<std::time::SystemTime>>,
    // Progress packets of every task touching this repo (SSE envelope).
    pub(crate) progress: ProgressTx,
    pub(crate) tasks: Arc<Tasks>,
    // Reporter of the task currently running inside sync (None = repo channel only).
    pub(crate) active_reporter: PLMutex<Option<Reporter>>,
}

impl RepoHandle {
    pub(crate) fn new(
        id: RepoId,
        local: LocalRepo,
        store: Prefixed,
        cfg: Arc<gitcask_config::Config>,
        manifest: Manifest,
        version: Option<Version>,
        state: RepoState,
        tasks: Arc<Tasks>,
    ) -> Self {
        let (progress, _) = tokio::sync::broadcast::channel(1024);
        RepoHandle {
            id,
            local,
            store,
            cfg,
            rw: TokioRwLock::new(()),
            sync_mutex: TokioMutex::new(()),
            pack_mutex: Arc::new(TokioMutex::new(())),
            manifest: PLRwLock::new(Arc::new(manifest)),
            manifest_version: PLMutex::new(version),
            state: PLMutex::new(state),
            last_freshness: PLMutex::new(None),
            last_access: PLMutex::new(Instant::now()),
            self_arc: std::sync::OnceLock::new(),
            publish_tx: PLMutex::new(None),
            publish_waiters: AtomicUsize::new(0),
            prefetch_inflight: std::sync::atomic::AtomicBool::new(false),
            last_entry_time: parking_lot::Mutex::new(None),
            progress,
            tasks,
            active_reporter: PLMutex::new(None),
        }
    }

    /// Live progress of everything happening to this repo on this instance.
    /// Subscribe *before* starting the work you want to watch.
    pub fn subscribe_progress(&self) -> ProgressRx {
        self.progress.subscribe()
    }

    /// Reporter for work done on behalf of this repo: the running sync task's
    /// (so packets land in its record) or the bare repo channel.
    pub fn reporter(&self) -> Reporter {
        self.active_reporter
            .lock()
            .clone()
            .unwrap_or_else(|| Reporter::for_repo(self.progress.clone()))
    }

    pub fn tasks(&self) -> &Arc<Tasks> {
        &self.tasks
    }

    pub fn progress_tx(&self) -> ProgressTx {
        self.progress.clone()
    }

    /// Begin a task of `kind` on this repo (lock per (repo, kind); packets
    /// mirrored into the repo channel).
    pub fn begin_task(&self, kind: &str, params: HashMap<String, String>) -> Begin {
        self.tasks.begin(
            &self.id.to_string(),
            kind,
            params,
            Some(self.progress.clone()),
        )
    }

    pub(crate) fn set_self_arc(&self, arc: Arc<RepoHandle>) {
        let _ = self.self_arc.set(arc);
    }

    // ---- public API ----

    pub fn id(&self) -> &RepoId {
        &self.id
    }

    pub fn local(&self) -> &LocalRepo {
        &self.local
    }

    pub fn store(&self) -> &Prefixed {
        &self.store
    }

    pub fn config(&self) -> &Arc<gitcask_config::Config> {
        &self.cfg
    }

    async fn try_maintenance_lease(
        &self,
        name: &str,
    ) -> Result<Option<gitcask_store::coord::LeaseGuard>, WalError> {
        let store: gitcask_store::DynStore = Arc::new(self.store.clone());
        Ok(gitcask_store::coord::try_acquire(
            store,
            &gitcask_proto::keys::lease_key(name),
            gitcask_store::coord::instance_id(),
            name,
            self.cfg.compaction.lease_ttl,
        )
        .await?)
    }

    /// Try to acquire this repository's compaction lease.
    pub async fn try_compaction_lease(
        &self,
    ) -> Result<Option<gitcask_store::coord::LeaseGuard>, WalError> {
        self.try_maintenance_lease("compact").await
    }

    /// Try to acquire this repository's checkpoint lease.
    pub async fn try_checkpoint_lease(
        &self,
    ) -> Result<Option<gitcask_store::coord::LeaseGuard>, WalError> {
        self.try_maintenance_lease("checkpoint").await
    }

    pub fn manifest(&self) -> Arc<Manifest> {
        self.manifest.read().clone()
    }

    pub fn manifest_version(&self) -> Option<Version> {
        self.manifest_version.lock().clone()
    }

    /// Last applied log entry sequence (local replay progress).
    pub fn applied_seq(&self) -> u64 {
        self.state.lock().applied_seq
    }

    /// Persisted manifest version string from the local state file.
    pub fn local_version(&self) -> Option<String> {
        self.state.lock().manifest_version.clone()
    }

    pub fn last_access(&self) -> Instant {
        *self.last_access.lock()
    }

    pub fn touch(&self) {
        *self.last_access.lock() = Instant::now();
    }

    /// Freshness check + full local catch-up (refs and every live pack).
    /// Returns a read guard; while any guard is alive no pack is removed
    /// locally. Required before anything that reads or verifies objects
    /// (upload-pack, receive-pack, maintenance and tooling).
    pub async fn sync_full(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        self.sync_level(SyncLevel::Full).await
    }

    /// Freshness check + refs-only catch-up: applies the WAL's ref state but
    /// downloads no packs. This is the cheap cold-start path for
    /// `info/refs`, `ls-refs` and the web `refs` endpoint.
    /// When packs are not yet reconciled and `wal.prefetch_packs` is on, a
    /// background full sync is kicked off so the first fetch finds them.
    pub async fn sync_refs(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        let guard = self.sync_refs_only().await?;
        if self.prefetch_wanted() {
            self.spawn_pack_prefetch();
        }
        Ok(guard)
    }

    /// Freshness check + refs-only catch-up without scheduling pack prefetch.
    /// Maintenance units whose inputs are only the manifest, log and refs use
    /// this path so checkpoint planning never materializes object data.
    pub async fn sync_refs_only(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        self.sync_level(SyncLevel::Refs).await
    }

    /// Whether a refs-level sync should pull the local copy in the background.
    pub fn prefetch_wanted(&self) -> bool {
        self.cfg.wal.prefetch_packs && !self.packs_ready()
    }

    /// True when the local pack set matches the last applied manifest.
    pub fn packs_ready(&self) -> bool {
        self.state.lock().packs_ready()
    }

    fn spawn_pack_prefetch(&self) {
        if self.prefetch_inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(arc) = self.self_arc.get().cloned() else {
            self.prefetch_inflight.store(false, Ordering::Release);
            return;
        };
        tokio::spawn(async move {
            let span = tracing::info_span!("wal.prefetch_packs", repo = %arc.id);
            if let Err(e) = arc.sync_impl_level(SyncLevel::Full).instrument(span).await {
                tracing::warn!(repo = %arc.id, error = ?e, "background pack prefetch failed");
            }
            arc.prefetch_inflight.store(false, Ordering::Release);
        });
    }

    async fn sync_level(&self, level: SyncLevel) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        let span = tracing::info_span!(
            "wal.sync",
            repo = %self.id,
            level = ?level,
            changed = false,
            entries_applied = 0u64,
        );
        // Maintenance planning comes through `sync_refs_only`, so processing a
        // pending marker also counts as access for cache eviction.
        self.touch();
        // Phase 1 — refs (manifest freshness + ref state), under sync_mutex
        // only; sub-second. Phase 2 — packs, under pack_mutex only. Neither
        // takes `rw.write()`: that lock exists solely so a superseded pack is
        // not unlinked under an active reader, and it is only ever
        // `try_write()`n (see sync.rs) — a queued writer on a tokio RwLock
        // blocks every *new* reader until all current readers are gone, and a
        // clone's ReadGuard lives for the whole stream (prod 2026-08-20: a
        // 24-minute clone + one queued writer = every info/refs on the
        // instance waited 60–680 s on `rw.read()`).
        self.sync_refs_phase(&span).instrument(span.clone()).await?;
        if level.wants_packs() {
            self.sync_packs_phase(level, &span)
                .instrument(span.clone())
                .await?;
        }
        let read_guard = crate::lockwait::timed(
            "rw.read",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.rw.try_read().ok(),
            self.rw.read().instrument(span.clone()),
        )
        .await;
        Ok(crate::sync::ReadGuard {
            _guard: read_guard,
            handle: self,
        })
    }

    /// Manifest freshness check + ref state apply (never packs, never
    /// `rw.write()`: ref files and the gix handle are replaced atomically).
    async fn sync_refs_phase(&self, span: &tracing::Span) -> Result<(), WalError> {
        if self.freshness_ttl_active() {
            return Ok(());
        }
        let _sync_guard = crate::lockwait::timed(
            "sync_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.sync_mutex.try_lock().ok(),
            self.sync_mutex.lock(),
        )
        .await;
        if self.freshness_ttl_active() {
            return Ok(());
        }
        self.sync_locked_inner(span).await
    }

    /// Make the local pack set match the manifest (download missing packs and
    /// drop superseded packs). Registers the `materialize` task.
    /// Serialized by `pack_mutex`; concurrent readers keep reading (packs are
    /// only added; removals take the write lock for the rename alone).
    async fn sync_packs_phase(
        &self,
        level: SyncLevel,
        _span: &tracing::Span,
    ) -> Result<(), WalError> {
        if self.level_satisfied(level) {
            return Ok(());
        }
        // A second caller of a running materialize waits here: the task join, measured.
        let pack_guard = crate::lockwait::timed(
            "pack_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.pack_mutex.clone().try_lock_owned().ok(),
            self.pack_mutex.clone().lock_owned(),
        )
        .await;
        if self.level_satisfied(level) {
            return Ok(());
        }
        let manifest = self.manifest();
        let task = match self.begin_task("materialize", HashMap::new()) {
            Begin::Started(t) => {
                *self.active_reporter.lock() = Some(t.reporter());
                Some(t)
            }
            Begin::AlreadyRunning(_) => None, // cannot happen: pack_mutex is held
        };
        // The whole materialization runs on the bulk runtime (own threads):
        // nothing in it can stall this runtime's request workers.
        let Some(arc) = self.self_arc.get().cloned() else {
            let work = async {
                crate::sync::reconcile_packs(self, &manifest).await?;
                self.local.refresh_async().await?;
                Ok::<(), WalError>(())
            };
            let res = match &task {
                Some(t) => work.instrument(t.span()).await,
                None => work.await,
            };
            *self.active_reporter.lock() = None;
            if let Some(task) = task {
                match &res {
                    Ok(()) => {
                        task.finish_ok(
                            format!("local copy complete at seq {}", self.applied_seq()),
                            None,
                        );
                    }
                    Err(error) => {
                        task.finish_err(500, error.to_string());
                    }
                }
            }
            drop(pack_guard);
            return res;
        };
        let m = manifest.clone();
        let task_span = task.as_ref().map(crate::tasks::TaskHandle::span);
        // The owned pack guard and task travel with the bulk work. If the
        // requesting future is cancelled, materialization keeps its
        // single-flight lock and records its real outcome instead of letting a
        // second install race its temp files.
        crate::sync::on_bulk_runtime(self.cfg.cache.bulk_threads, async move {
            let work = async {
                crate::sync::reconcile_packs(&arc, &m).await?;
                arc.local.refresh_async().await?;
                Ok::<(), WalError>(())
            };
            let res = match task_span {
                Some(sp) => work.instrument(sp).await,
                None => work.await,
            };
            *arc.active_reporter.lock() = None;
            if let Some(task) = task {
                match &res {
                    Ok(()) => {
                        task.finish_ok(
                            format!("local copy complete at seq {}", arc.applied_seq()),
                            None,
                        );
                    }
                    Err(error) => {
                        task.finish_err(500, error.to_string());
                    }
                }
            }
            drop(pack_guard);
            res
        })
        .await
    }

    /// Freshness check + refs apply, with `sync_mutex` and the write lock held
    /// by the caller. Packs are never touched here (see `sync_packs_phase`).
    async fn sync_locked_inner(&self, span: &tracing::Span) -> Result<(), WalError> {
        let known = self.manifest_version.lock().clone();
        let outcome = crate::sync::freshness_check(&self.store, &known).await?;
        match outcome {
            crate::sync::SyncOutcome::Unchanged => self.update_freshness(),
            crate::sync::SyncOutcome::Changed {
                meta_version,
                manifest,
            } => {
                // Monotonic: never apply a manifest older than the one this instance already holds. A
                // publish on this instance commits locally (manifest, version, refs) outside this lock, so
                // a sync that read the manifest just before that CAS can arrive here with the previous
                // revision — applying it rewrote packed-refs to the pre-push state and rolled the known
                // version back, so one `ls-remote` right after an acknowledged push answered the OLD tip
                // (a concurrency regression test; the next request's conditional GET then
                // repaired it). The revision increments on every manifest write.
                let cur = self.manifest();
                let initialised = self.manifest_version.lock().is_some();
                if manifest.revision < cur.revision {
                    tracing::debug!(repo = %self.id, read_rev = manifest.revision, held_rev = cur.revision, "stale manifest read ignored (a local publish is ahead)");
                    self.update_freshness();
                    return Ok(());
                }
                if initialised && manifest.revision == cur.revision {
                    // Same content under a version we did not record (a publish that learned the version
                    // by HEAD): adopt the version so the next check is a 304, apply nothing.
                    *self.manifest_version.lock() = Some(meta_version);
                    self.update_freshness();
                    return Ok(());
                }
                span.record("changed", true);
                let before = self.state.lock().applied_seq;
                crate::sync::apply_delta(self, &manifest, &meta_version).await?;
                span.record("entries_applied", manifest.head_seq.saturating_sub(before));
                *self.manifest.write() = Arc::new(manifest);
                *self.manifest_version.lock() = Some(meta_version);
                self.update_freshness();
            }
        }
        Ok(())
    }

    fn level_satisfied(&self, level: SyncLevel) -> bool {
        match level {
            SyncLevel::Refs => true,
            SyncLevel::Full => self.packs_ready(),
        }
    }

    /// Internal serving sync (no read guard). Used by publish/checkpoint/read_log.
    pub(crate) async fn sync_impl(&self) -> Result<(), WalError> {
        self.sync_impl_level(SyncLevel::Full).await
    }

    pub(crate) async fn sync_impl_level(&self, level: SyncLevel) -> Result<(), WalError> {
        let span = tracing::info_span!("wal.sync", repo = %self.id, level = ?level, changed = false, entries_applied = 0u64);
        self.sync_refs_phase(&span).instrument(span.clone()).await?;
        if level.wants_packs() {
            self.sync_packs_phase(level, &span)
                .instrument(span.clone())
                .await?;
        }
        Ok(())
    }

    /// Force full re-materialize from store (repair).
    pub async fn rematerialize(&self) -> Result<(), WalError> {
        let _sync_guard = crate::lockwait::timed(
            "sync_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.sync_mutex.try_lock().ok(),
            self.sync_mutex.lock(),
        )
        .await;
        let _pack_guard = crate::lockwait::timed(
            "pack_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.pack_mutex.try_lock().ok(),
            self.pack_mutex.lock(),
        )
        .await;

        // Read manifest fresh
        let (meta, manifest) = match gitcask_store::coord::get_message::<Manifest>(
            &self.store,
            gitcask_proto::keys::MANIFEST,
        )
        .await?
        {
            Some((m, manifest)) => (m, manifest),
            None => return Err(WalError::NotFound),
        };

        // Reset state and re-materialize
        crate::sync::materialize_from_scratch(self, &manifest, &meta.version).await?;

        *self.manifest.write() = Arc::new(manifest);
        *self.manifest_version.lock() = Some(meta.version);
        self.last_freshness.lock().take();

        Ok(())
    }

    /// Publish a push.
    pub async fn publish_push(
        &self,
        pack: Option<gitcask_git::IngestedPack>,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish(pack, txn, meta, false).await
    }

    /// Publish a push when the caller has already completed the request's
    /// freshness sync (`sync_full()` for receive-pack, `sync_refs()` for a
    /// ref-only mutation).
    ///
    /// Receive-pack holds a read guard while parsing and ingesting the pack.
    /// Reusing that freshness check avoids a second conditional manifest GET
    /// before the publisher's first CAS attempt. The publisher still syncs
    /// after every CAS conflict.
    pub async fn publish_push_synced(
        &self,
        pack: Option<gitcask_git::IngestedPack>,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish(pack, txn, meta, true).await
    }

    /// Publish with an explicit entry time (history replay into the WAL): the
    /// log entry's `created_at` is `at` instead of now, validated monotonic
    /// (≥ the head entry's time; ≥ earlier explicit times in the same batch),
    /// else every ref of the transaction is rejected with the reason. The WAL
    /// itself never enforces fast-forward — that is receive-pack — so a
    /// replay may move `main` non-ancestrally between slots; callers pass
    /// `old_oid` = the current value (or "" to skip the old check).
    pub async fn publish_push_at(
        &self,
        pack: Option<gitcask_git::IngestedPack>,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        at: std::time::SystemTime,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish_at(
            pack,
            txn,
            meta,
            false,
            Some(gitcask_proto::time::from_system(at)),
        )
        .await
    }

    async fn enqueue_publish(
        &self,
        pack: Option<gitcask_git::IngestedPack>,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        synced: bool,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish_at(pack, txn, meta, synced, None).await
    }

    async fn enqueue_publish_at(
        &self,
        pack: Option<gitcask_git::IngestedPack>,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        synced: bool,
        created_at: Option<prost_types::Timestamp>,
    ) -> Result<PublishResult, WalError> {
        self.publish_waiters.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = PublishRequest {
            pack,
            txn,
            meta,
            synced,
            created_at,
            response: tx,
        };

        let sender = self.get_or_init_publisher();
        if sender.send(request).is_err() {
            self.publish_waiters.fetch_sub(1, Ordering::Relaxed);
            return Err(WalError::Corrupt("publisher channel closed".into()));
        }

        let result = rx
            .await
            .map_err(|_| WalError::Corrupt("publisher dropped response".into()))?;
        self.publish_waiters.fetch_sub(1, Ordering::Relaxed);
        result
    }
    /// Publish a ref-only update (no pack).
    pub async fn publish_ref_update(
        &self,
        txn: gitcask_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.publish_push(None, txn, meta).await
    }

    /// Publish a compact entry.
    pub async fn publish_compact(
        &self,
        new_pack: gitcask_git::PackInfo,
        supersedes: Vec<gix_hash::ObjectId>,
        tier: u32,
    ) -> Result<u64, WalError> {
        crate::publish::publish_compact_impl(self, new_pack, supersedes, tier).await
    }

    /// Write checkpoint at current head.
    pub async fn write_checkpoint(&self) -> Result<gitcask_proto::v1::CheckpointRef, WalError> {
        crate::checkpoint::write_checkpoint_impl(self).await
    }

    /// Attach side-files (rev/bitmap/commit-graph) to a published pack and
    /// advertise them in the manifest (CAS). See `publish::annotate_pack_impl`.
    pub async fn annotate_pack(
        &self,
        checksum: &str,
        rev: Option<std::path::PathBuf>,
        bitmap: Option<std::path::PathBuf>,
        commit_graph: Option<std::path::PathBuf>,
    ) -> Result<gitcask_proto::v1::PackRef, WalError> {
        crate::publish::annotate_pack_impl(self, checksum, rev, bitmap, commit_graph).await
    }

    /// Publish an already built pack (`pack-<checksum>.pack` + `.idx`) as a
    /// tier-`tier` COMPACT entry superseding nothing.
    pub async fn add_pack(
        &self,
        pack: &std::path::Path,
        idx: &std::path::Path,
        tier: u32,
    ) -> Result<u64, WalError> {
        crate::publish::add_pack_impl(self, pack, idx, tier).await
    }

    /// Whether the last known manifest wants a checkpoint, and why.
    pub fn checkpoint_due(&self) -> Option<crate::checkpoint::CheckpointTrigger> {
        crate::checkpoint::checkpoint_due(&self.manifest(), &self.cfg.wal)
    }

    /// Download `wal/<checksum>.pack` + `.idx` (+ advertised side-files) from the
    /// store into `dir` as `pack-<checksum>.*` (striped), for tooling that
    /// rebuilds a historical copy elsewhere (`gitcask wal materialize`). The
    /// live local copy is never touched.
    pub async fn fetch_pack_into(
        &self,
        pack: &gitcask_proto::v1::PackRef,
        dir: &std::path::Path,
    ) -> Result<(), WalError> {
        tokio::fs::create_dir_all(dir).await?;
        let c = &pack.checksum;
        let pack_path = dir.join(format!("pack-{c}.pack"));
        let idx_path = dir.join(format!("pack-{c}.idx"));
        let pack_key = gitcask_proto::keys::pack_key(c);
        let idx_key = gitcask_proto::keys::idx_key(c);
        let pack_size = pack.pack_size;
        let idx_size = pack.idx_size;
        let pack_fut = async {
            crate::sync::download_object(
                &self.store,
                &pack_key,
                &pack_path,
                (pack_size > 0).then_some(pack_size),
                None,
            )
            .await
        };
        let idx_fut = async {
            crate::sync::download_object(
                &self.store,
                &idx_key,
                &idx_path,
                (idx_size > 0).then_some(idx_size),
                None,
            )
            .await
        };
        let mut side_futs = Vec::new();
        for (flag, ext, key) in [
            (pack.has_rev, "rev", gitcask_proto::keys::rev_key(c)),
            (
                pack.has_bitmap,
                "bitmap",
                gitcask_proto::keys::bitmap_key(c),
            ),
            (
                pack.has_commit_graph,
                "commit-graph",
                gitcask_proto::keys::commit_graph_key(c),
            ),
        ] {
            if flag {
                let store = self.store.clone();
                let path = dir.join(format!("pack-{c}.{ext}"));
                side_futs.push(async move {
                    crate::sync::download_object(&store, &key, &path, None, None).await
                });
            }
        }
        let (pack_r, idx_r, sides) =
            tokio::join!(pack_fut, idx_fut, futures::future::join_all(side_futs));
        pack_r?;
        idx_r?;
        let _ = sides;
        Ok(())
    }

    /// Read log entries [from_seq, to_seq].
    pub async fn read_log(
        &self,
        from_seq: u64,
        to_seq: Option<u64>,
    ) -> Result<Vec<gitcask_proto::v1::LogEntry>, WalError> {
        crate::log_reader::read_log_impl(self, from_seq, to_seq).await
    }

    // ---- internal helpers ----

    fn freshness_ttl_active(&self) -> bool {
        let ttl = self.cfg.wal.freshness_ttl;
        if ttl == std::time::Duration::ZERO {
            return false;
        }
        let last = self.last_freshness.lock();
        match *last {
            Some(t) => t.elapsed() < ttl,
            None => false,
        }
    }

    fn update_freshness(&self) {
        *self.last_freshness.lock() = Some(Instant::now());
    }

    fn get_or_init_publisher(&self) -> mpsc::UnboundedSender<PublishRequest> {
        let mut guard = self.publish_tx.lock();
        if let Some(tx) = &*guard {
            // A publisher task that died (panic mid-batch) leaves a sender to
            // a dropped receiver; respawn instead of failing every push on
            // this instance forever.
            if !tx.is_closed() {
                return tx.clone();
            }
            tracing::warn!(repo = %self.id, "publisher task is gone; respawning");
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let arc = self
            .self_arc
            .get()
            .expect("self_arc must be set before publish")
            .clone();
        tokio::spawn(crate::publish::publisher_task(arc, rx));
        *guard = Some(tx.clone());
        tx
    }
}
