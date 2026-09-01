//! The `maintain` role: a **permanent, self-healing priority loop** driven by
//! bucket-root `pending/<owner>/<repo>` markers. For each marked repository it
//! runs bounded units as tasks (discoverable at `…/tasks`, visible on the WAL
//! page), with compacting repositories running to idle and checkpoint-only
//! repositories yielding after one unit:
//!
//! 1. checkpoint-if-due (refs-level; works for any repo on any host),
//! 2. geometric compaction when triggered,
//! 3. missing reverse indexes,
//! 4. connectivity audit when due,
//! 5. bucket GC after a new compaction or checkpoint, then shared-cache expiry
//!    from the same repository listing.
//!
//! Each pass writes a heartbeat (`maintain/<host>.pb`) so operators can see
//! which maintainers are alive.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use futures::StreamExt;
use gitcask_git::RepoId;
use gitcask_store::{ObjectMeta, PutBody, PutMode, PutOptions, StoreError};
use parking_lot::Mutex;
use prost::Message;
use tracing::{Instrument, info, warn};

use crate::AppState;

/// Run forever: wait `maintenance.interval` only when the pending queue is
/// empty; otherwise start another bounded pass after at least one second.
pub async fn run_loop(state: Arc<AppState>) {
    let interval = state.cfg.maintenance.interval;
    let started = SystemTime::now();
    let host = host_name(&state);
    info!(interval = ?interval, workers = state.cfg.maintenance.workers, max_repos_per_pass = state.cfg.maintenance.max_repos_per_pass, host = %host, "maintenance loop started");
    let mut passes = 0u64;
    let mut last_unit = String::new();
    let mut wait = std::time::Duration::ZERO;
    let mut next_heartbeat_gc = Instant::now();
    loop {
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        if gitcask_wal::tasks::draining() {
            info!("maintenance loop: draining, no new pass");
            return;
        }
        passes += 1;
        let t0 = Instant::now();
        if Instant::now() >= next_heartbeat_gc {
            next_heartbeat_gc = Instant::now() + HEARTBEAT_GC_INTERVAL;
            if let Err(error) = heartbeats(&state).await {
                warn!(%error, "maintenance: heartbeat GC failed");
            }
        }
        // `maintain.pass`: one close line per pass with the counts; every unit
        // (and its task.run) is a child, so a trace holds the whole pass.
        let span = tracing::info_span!("maintain.pass", host = %host, pass = passes, repos = tracing::field::Empty, units = tracing::field::Empty, skipped = tracing::field::Empty, outcome = tracing::field::Empty);
        // Heartbeat DURING the pass too: a long unit (for example, a 1 h
        // rev-index over a 32 GB pack) otherwise shows the host STALE while it
        // is working.
        let pass = run_pass(&state).instrument(span.clone());
        tokio::pin!(pass);
        let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_mins(2));
        heartbeat_interval.tick().await;
        let outcome = loop {
            tokio::select! {
                outcome = &mut pass => break outcome,
                _ = heartbeat_interval.tick() => {
                    if let Err(e) = heartbeat(&state, &host, started, passes, &last_unit).await {
                        warn!(error = %e, "maintenance heartbeat (mid-pass) failed");
                    }
                }
            }
        };
        let retry_soon = matches!(&outcome, Ok(report) if report.markers > 0);
        match outcome {
            Ok(r) => {
                if let Some(u) = &r.last_unit {
                    last_unit = u.clone();
                }
                span.record("repos", r.repos);
                span.record("units", r.units);
                span.record("skipped", r.skipped);
                span.record("outcome", "ok");
                if r.units > 0 {
                    info!(
                        repos = r.repos,
                        units = r.units,
                        checkpoints = r.checkpoints,
                        compactions = r.compactions,
                        gcs = r.gcs,
                        "maintenance pass"
                    );
                }
            }
            Err(e) => {
                span.record("outcome", "error");
                warn!(error = %e, "maintenance pass failed")
            }
        }
        metrics::histogram!("gitcask_maintain_pass_seconds", "host" => host.clone())
            .record(t0.elapsed().as_secs_f64());
        if let Err(e) = heartbeat(&state, &host, started, passes, &last_unit).await {
            warn!(error = %e, "maintenance heartbeat failed");
        }
        wait = if retry_soon {
            std::time::Duration::from_secs(1)
        } else {
            interval
        };
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PassReport {
    /// Pending markers returned by this bounded list.
    pub markers: usize,
    pub repos: usize,
    pub units: usize,
    pub checkpoints: usize,
    pub compactions: usize,
    pub gcs: usize,
    pub last_unit: Option<String>,
    /// Repos that had a unit this host could not run (wrong-host/too-small/blocked are not counted here; planning errors are).
    pub skipped: u64,
}

impl PassReport {
    fn merge(&mut self, other: PassReport) {
        self.repos += other.repos;
        self.units += other.units;
        self.checkpoints += other.checkpoints;
        self.compactions += other.compactions;
        self.gcs += other.gcs;
        self.skipped += other.skipped;
        if other.last_unit.is_some() {
            self.last_unit = other.last_unit;
        }
    }
}

/// What this host would do for `id` right now (the first unit of the
/// priority order), or why nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum Unit {
    Checkpoint(String),
    Compact,
    /// An installed pack the manifest advertises without a `.rev` (written by
    /// git < 2.41, or imported without one): build it here, upload it as the
    /// side-file, CAS it into the manifest — every other host then downloads
    /// it on its next sync instead of rebuilding 60 M entries per fetch.
    RevIndex(String),
    /// Connectivity audit due after compaction, or after the configured age
    /// when a prior audit exists and the WAL has advanced.
    Fsck(String),
    /// Collect superseded packs, folded logs, old checkpoints, and expired
    /// shared cache objects after all integrity work for the current WAL
    /// generation is complete.
    Gc(String),
    /// Nothing to do.
    Idle,
}

/// Packs with at least this many objects get a `.rev` side-file (≈ 50 ns per
/// object per `pack-objects` without one: 60 M → 2.85 s, 250 k → 12 ms).
pub const REV_INDEX_MIN_OBJECTS: u64 = 250_000;

pub fn host_name(state: &AppState) -> String {
    state
        .cfg
        .maintenance
        .host
        .clone()
        .unwrap_or_else(|| gitcask_store::coord::instance_id().to_string())
}

/// The next unit for `id` on this host (pure w.r.t. side effects except a
/// refs sync and integrity-report read).
pub async fn next_unit(state: &Arc<AppState>, id: &RepoId) -> anyhow::Result<Unit> {
    let handle = state.registry.open(id).await?;
    handle.sync_refs_only().await?;
    let cfg = state.cfg.clone();
    {
        // Checkpoint lag/age gauges: how far the fold is behind the head.
        let m = handle.manifest();
        let cp_seq = m.checkpoint.as_ref().map(|c| c.seq).unwrap_or(0);
        metrics::gauge!("gitcask_checkpoint_lag_entries", "repo" => id.to_string())
            .set(m.head_seq.saturating_sub(cp_seq) as f64);
        if let Some(t) = m
            .checkpoint
            .as_ref()
            .and_then(|c| c.created_at.as_ref())
            .map(gitcask_proto::time::to_system)
        {
            metrics::gauge!("gitcask_checkpoint_age_seconds", "repo" => id.to_string()).set(
                SystemTime::now()
                    .duration_since(t)
                    .unwrap_or_default()
                    .as_secs_f64(),
            );
        }
    }
    if cfg.maintenance.checkpoints {
        if let Some(trigger) = handle.checkpoint_due() {
            return Ok(Unit::Checkpoint(trigger.to_string()));
        }
    }
    // Integrity before everything else that builds on the object set.
    let fsck = crate::ops::read_fsck(&handle).await.ok().flatten();
    if let Some(f) = &fsck {
        metrics::gauge!("gitcask_repo_missing_objects", "repo" => id.to_string())
            .set(f.missing_total as f64);
    }
    if cfg.compaction.enabled && state.cfg.has_role(gitcask_config::Role::Compact) {
        if crate::ops::compaction_triggered(&handle, &cfg) {
            return Ok(Unit::Compact);
        }
    }
    // A big pack without its `.rev` side-file. Push packs (gix ingest, no .rev)
    // stay as they are: git's in-memory
    // reverse index costs ~50 ns/object per pack-objects, nothing below the
    // threshold; a side-file per push would be manifest churn for no gain.
    if state.cfg.has_role(gitcask_config::Role::Compact) {
        let m = handle.manifest();
        if let Some(p) = m
            .packs
            .iter()
            .filter(|p| !p.has_rev && p.object_count >= REV_INDEX_MIN_OBJECTS)
            .min_by_key(|p| p.seq)
            && let Ok(oid) = gix_hash::ObjectId::from_hex(p.checksum.as_bytes())
            && handle.local().pack_path(&oid).exists()
        {
            return Ok(Unit::RevIndex(p.checksum.clone()));
        }
    }
    // Lowest priority: audit every compacted pack set, then audit an already
    // audited repository by age only when its WAL advanced in the meantime.
    // A young repository with no compacted pack and no fsck.pb was already
    // checked on ingest, so its first pending-marker visit stays refs-only.
    let interval = cfg.maintenance.fsck_interval;
    let manifest = handle.manifest();
    let head_seq = manifest.head_seq;
    let last_compact_seq = manifest
        .packs
        .iter()
        .filter(|pack| pack.tier > 0)
        .map(|pack| pack.seq)
        .max()
        .unwrap_or(0);
    let audited_seq = fsck.as_ref().map_or(0, |report| report.audited_seq);
    if last_compact_seq > audited_seq {
        return Ok(Unit::Fsck(format!(
            "compaction at seq {last_compact_seq} not audited"
        )));
    }
    if let Some(report) = &fsck {
        let audited_at = report
            .at
            .as_ref()
            .map(gitcask_proto::time::to_system)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let age = SystemTime::now()
            .duration_since(audited_at)
            .unwrap_or_default();
        if head_seq > audited_seq && age > interval {
            return Ok(Unit::Fsck(format!(
                "last audit {}h ago; WAL advanced from seq {audited_seq} to {head_seq}",
                age.as_secs() / 3600
            )));
        }
    }
    if let Some(trigger) = crate::gc::due(&handle).await.map_err(anyhow::Error::msg)? {
        return Ok(Unit::Gc(trigger.to_string()));
    }
    Ok(Unit::Idle)
}

/// One bounded, oldest-first pass over pending markers. Repositories run in
/// parallel, while each repository's priority planner remains serial.
pub async fn run_pass(state: &Arc<AppState>) -> anyhow::Result<PassReport> {
    let mut markers = list_pending(state).await?;
    markers.sort_by_key(|marker| marker.last_modified);
    let marker_count = markers.len();
    metrics::gauge!("gitcask_pending_markers")
        .set(f64::from(u32::try_from(marker_count).unwrap_or(u32::MAX)));
    if marker_count == state.cfg.maintenance.max_repos_per_pass {
        // ObjectStore's streaming LIST hides backend page tokens. Filling the
        // bounded page is the portable signal that the next pass may have work.
        info!(
            markers = marker_count,
            remaining_markers_estimate = "at least one",
            "maintenance: pending marker page full; backlog remains"
        );
    }

    let report = Arc::new(Mutex::new(PassReport {
        markers: marker_count,
        ..PassReport::default()
    }));
    let busy = metrics::gauge!("gitcask_maintain_workers_busy", "host" => host_name(state));
    busy.set(0.0);
    futures::stream::iter(markers)
        .for_each_concurrent(state.cfg.maintenance.workers, |marker| {
            let state = state.clone();
            let report = report.clone();
            let busy = busy.clone();
            async move {
                // A queued future starts only when a worker slot is free. Check
                // draining here so no newly available slot starts another repo.
                if gitcask_wal::tasks::draining() {
                    return;
                }
                busy.increment(1.0);
                let worker_report = run_marker(&state, marker).await;
                busy.decrement(1.0);
                report.lock().merge(worker_report);
            }
        })
        .await;

    let report = report.lock().clone();
    metrics::gauge!("gitcask_maintain_pass_repos")
        .set(f64::from(u32::try_from(report.repos).unwrap_or(u32::MAX)));
    Ok(report)
}

async fn run_marker(state: &Arc<AppState>, marker: ObjectMeta) -> PassReport {
    let mut report = PassReport::default();
    let id = match pending_repo(&marker.key) {
        Ok(id) => id,
        Err(error) => {
            warn!(key = %marker.key, %error, "maintenance: deleting malformed pending marker");
            delete_marker(state, &marker).await;
            return report;
        }
    };
    match state.registry.open(&id).await {
        Ok(_) => {}
        Err(gitcask_wal::WalError::NotFound) => {
            delete_marker(state, &marker).await;
            return report;
        }
        Err(error) => {
            warn!(repo = %id, %error, "maintenance: opening repository failed");
            report.skipped += 1;
            return report;
        }
    }
    report.repos = 1;
    let mut complete = true;
    let mut checkpoint_only = false;
    loop {
        if gitcask_wal::tasks::draining() {
            complete = false;
            break;
        }
        let unit = match next_unit(state, &id).await {
            Ok(unit) => unit,
            Err(error) => {
                warn!(repo = %id, %error, "maintenance: planning failed");
                report.skipped += 1;
                complete = false;
                break;
            }
        };
        if matches!(unit, Unit::Idle) {
            // A checkpoint-only repository yields after its one refs-level
            // unit and keeps its marker. If the next plan was Compact, it is a
            // hot repository: clear this flag and continue all the way to idle.
            if checkpoint_only {
                complete = false;
            }
            break;
        }
        // `fsck` needs a complete local copy. Keep a checkpoint-only
        // pass refs-only: preserve the marker so the lowest-priority audit
        // runs first on the next pass, rather than turning successful
        // higher-priority work into pack prefetch.
        if checkpoint_only && matches!(unit, Unit::Fsck(_) | Unit::Gc(_)) {
            complete = false;
            break;
        }
        if !run_unit(state, &id, &unit, &mut report).await {
            report.skipped += 1;
            complete = false;
            break;
        }
        if matches!(unit, Unit::Checkpoint(_)) && report.units == 1 {
            checkpoint_only = true;
        } else {
            checkpoint_only = false;
        }
    }
    if complete {
        delete_marker(state, &marker).await;
    }
    report
}

async fn list_pending(state: &AppState) -> anyhow::Result<Vec<ObjectMeta>> {
    let mut stream = state.store.list(gitcask_proto::keys::PENDING_DIR, None);
    let mut markers = Vec::with_capacity(state.cfg.maintenance.max_repos_per_pass);
    while markers.len() < state.cfg.maintenance.max_repos_per_pass {
        let Some(marker) = stream.next().await else {
            break;
        };
        markers.push(marker?);
    }
    Ok(markers)
}

fn pending_repo(key: &str) -> Result<RepoId, String> {
    let value = key
        .strip_prefix(gitcask_proto::keys::PENDING_DIR)
        .ok_or_else(|| "key is outside pending/".to_string())?;
    let (owner, name) = value
        .split_once('/')
        .ok_or_else(|| "expected pending/<owner>/<repo>".to_string())?;
    RepoId::new(owner, name).map_err(|error| error.to_string())
}

async fn delete_marker(state: &AppState, marker: &ObjectMeta) {
    match state
        .store
        .delete(&marker.key, Some(marker.version.clone()))
        .await
    {
        Ok(()) | Err(StoreError::NotFound { .. }) => {}
        Err(StoreError::PreconditionFailed { .. }) => {
            tracing::debug!(key = %marker.key, "maintenance: pending marker changed during the pass");
        }
        Err(error) => {
            warn!(key = %marker.key, %error, "maintenance: deleting pending marker failed");
        }
    }
}

async fn run_unit(
    state: &Arc<AppState>,
    id: &RepoId,
    unit: &Unit,
    report: &mut PassReport,
) -> bool {
    let kind = match unit {
        Unit::Checkpoint(_) => "checkpoint",
        Unit::Compact => "compact",
        Unit::RevIndex(_) => "rev-index",
        Unit::Fsck(_) => "fsck",
        Unit::Gc(_) => "gc",
        Unit::Idle => return true,
    };
    let unit_span =
        tracing::info_span!("maintain.unit", repo = %id, kind, outcome = tracing::field::Empty);
    let t_unit = Instant::now();
    let done = async {
        match unit {
            Unit::Checkpoint(trigger) => {
                let mut params = HashMap::new();
                params.insert("trigger".to_string(), trigger.clone());
                let ok = run_op(state, id, "checkpoint", params).await;
                if ok {
                    report.checkpoints += 1;
                }
                ok
            }
            Unit::Compact => {
                let value = run_op_value(state, id, "compact", HashMap::new()).await;
                let lease_held = value.as_ref().is_some_and(|value| {
                    value.get("outcome").and_then(serde_json::Value::as_str) == Some("lease_held")
                });
                let ok = value.is_some() && !lease_held;
                if ok {
                    report.compactions += 1;
                }
                ok
            }
            Unit::RevIndex(checksum) => {
                let mut params = HashMap::new();
                params.insert("pack".to_string(), checksum.clone());
                run_op(state, id, "rev-index", params).await
            }
            Unit::Fsck(why) => {
                let mut params = HashMap::new();
                params.insert("connectivity".to_string(), "1".to_string());
                params.insert("why".to_string(), why.clone());
                run_op(state, id, "fsck", params).await
            }
            Unit::Gc(why) => {
                let mut params = HashMap::new();
                params.insert("why".to_string(), why.clone());
                let ok = run_op(state, id, "gc", params).await;
                if ok {
                    report.gcs += 1;
                }
                ok
            }
            Unit::Idle => true,
        }
    }
    .instrument(unit_span.clone())
    .await;
    let outcome = if done { "ok" } else { "failed" };
    unit_span.record("outcome", outcome);
    metrics::counter!("gitcask_maintain_units_total", "host" => host_name(state), "kind" => kind, "outcome" => outcome).increment(1);
    metrics::histogram!("gitcask_maintain_unit_seconds", "kind" => kind)
        .record(t_unit.elapsed().as_secs_f64());
    if done {
        report.units += 1;
        report.last_unit = Some(format!("{id} {unit:?}"));
    }
    done
}

/// Stale-heartbeat collection is operational housekeeping, not repository
/// work. Bound its LIST rate independently from a busy pending queue.
const HEARTBEAT_GC_INTERVAL: std::time::Duration = std::time::Duration::from_mins(10);

/// Every maintainer heartbeat in the bucket (expired ones purged).
pub async fn heartbeats(
    state: &AppState,
) -> anyhow::Result<Vec<gitcask_proto::v1::MaintainerHeartbeat>> {
    use futures::StreamExt;
    use gitcask_store::ObjectStoreExt;
    let mut out = Vec::new();
    let mut keys = state.store.list(gitcask_proto::keys::MAINTAIN_DIR, None);
    while let Some(m) = keys.next().await {
        let m = m?;
        if let Some((meta, bytes)) = state.store.get_bytes(&m.key).await? {
            if let Ok(hb) = gitcask_proto::v1::MaintainerHeartbeat::decode(bytes.as_ref()) {
                // A host that has not passed within the configured TTL is
                // gone: purge its heartbeat so operational views show only
                // live maintainers.
                let age = hb
                    .last_pass_at
                    .as_ref()
                    .map(gitcask_proto::time::to_system)
                    .and_then(|t| SystemTime::now().duration_since(t).ok());
                if age.is_some_and(|a| a > state.cfg.maintenance.heartbeat_ttl) {
                    if state.cfg.has_role(gitcask_config::Role::Maintain) {
                        info!(host = %hb.host, age_secs = age.map(|a| a.as_secs()).unwrap_or(0), "maintenance: purging expired heartbeat");
                        match state.store.delete(&m.key, Some(meta.version)).await {
                            Ok(()) | Err(StoreError::NotFound { .. }) => {}
                            Err(StoreError::PreconditionFailed { .. }) => {
                                tracing::debug!(key = %m.key, "maintenance: heartbeat changed during GC");
                            }
                            Err(error) => {
                                warn!(key = %m.key, %error, "maintenance: deleting expired heartbeat failed");
                            }
                        }
                    }
                    continue;
                }
                out.push(hb);
            }
        }
    }
    Ok(out)
}

/// Remove this instance's heartbeat after its maintainer loop has stopped.
pub async fn remove_heartbeat(state: &AppState) {
    let host = host_name(state);
    let key = gitcask_proto::keys::maintainer_key(&host);
    match state.store.delete(&key, None).await {
        Ok(()) | Err(StoreError::NotFound { .. }) => {
            info!(%host, "maintenance: removed heartbeat on shutdown");
        }
        Err(error) => {
            warn!(%host, %error, "maintenance: removing heartbeat on shutdown failed");
        }
    }
}

async fn heartbeat(
    state: &Arc<AppState>,
    host: &str,
    started: SystemTime,
    passes: u64,
    last_unit: &str,
) -> anyhow::Result<()> {
    metrics::gauge!("gitcask_maintainer_heartbeat_timestamp", "host" => host.to_string()).set(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64(),
    );
    let hb = gitcask_proto::v1::MaintainerHeartbeat {
        host: host.to_string(),
        repos: Vec::new(),
        exclude: Vec::new(),
        max_pack_bytes: 0,
        disk: "local".into(),
        started_at: Some(gitcask_proto::time::from_system(started)),
        last_pass_at: Some(gitcask_proto::time::now()),
        last_unit: last_unit.to_string(),
        passes,
    };
    state
        .store
        .put(
            &gitcask_proto::keys::maintainer_key(host),
            PutBody::Bytes(hb.encode_to_vec().into()),
            PutOptions::from(PutMode::Overwrite),
        )
        .await?;
    Ok(())
}

/// Start `op` as a task and wait for it. Returns true when it finished ok.
async fn run_op(
    state: &Arc<AppState>,
    id: &RepoId,
    op: &str,
    params: HashMap<String, String>,
) -> bool {
    run_op_value(state, id, op, params).await.is_some()
}

/// Like [`run_op`], returning the op's result value (`None` = failed / still running).
async fn run_op_value(
    state: &Arc<AppState>,
    id: &RepoId,
    op: &str,
    params: HashMap<String, String>,
) -> Option<serde_json::Value> {
    let started = Instant::now();
    let task = match crate::ops::start(state.clone(), id.clone(), op, params).await {
        Ok(t) => t,
        Err(crate::ops::StartError::AlreadyRunning(t)) => t,
        Err(crate::ops::StartError::UnknownOp) => {
            warn!(repo = %id, op, "maintenance: cannot start op");
            return None;
        }
    };
    // Bounded: a maintenance op that runs longer than an hour is reported and
    // left running (it stays discoverable at …/tasks); the pass moves on.
    if !task.wait_done(std::time::Duration::from_secs(3600)).await {
        warn!(repo = %id, op, "maintenance: op still running after 1h; moving on");
        return None;
    }
    match task.outcome() {
        Some(Ok(o)) => {
            info!(repo = %id, op, ms = started.elapsed().as_millis() as u64, "maintenance: done");
            Some(o.value.unwrap_or(serde_json::Value::Null))
        }
        Some(Err((_, msg))) => {
            warn!(repo = %id, op, error = %msg, "maintenance: failed");
            None
        }
        None => None,
    }
}
