//! Per-repository bucket garbage collection derived from the manifest, WAL,
//! and retained checkpoints. It is maintenance work only: candidate discovery
//! lists one already-pending repository prefix, never all repositories.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::{StreamExt, TryStreamExt};
use gitcask_proto::keys;
use gitcask_proto::v1::{Checkpoint, EntryKind, GcState, LogEntry, Manifest};
use gitcask_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, PutBody, PutMode, PutOptions, StoreError, Version,
};
use gitcask_wal::RepoHandle;
use prost::Message;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GcTrigger {
    pub compact_seq: u64,
    pub checkpoint_seq: u64,
}

impl std::fmt::Display for GcTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "compaction seq {}, checkpoint seq {}",
            self.compact_seq, self.checkpoint_seq
        )
    }
}

#[derive(Debug, Default, Serialize)]
pub struct GcOutcome {
    pub packs: usize,
    pub logs: usize,
    pub checkpoints: usize,
    pub caches: usize,
    pub objects: usize,
    pub trigger: GcTrigger,
}

#[derive(Default)]
struct GcPlan {
    packs: Vec<Vec<ObjectMeta>>,
    logs: Vec<ObjectMeta>,
    checkpoints: Vec<Vec<ObjectMeta>>,
    caches: Vec<ObjectMeta>,
}

#[derive(Default)]
struct DeleteCounts {
    packs: usize,
    logs: usize,
    checkpoints: usize,
    caches: usize,
    objects: usize,
}

#[derive(Clone, Copy)]
enum GcKind {
    Pack,
    Log,
    Checkpoint,
    Cache,
}

impl GcKind {
    fn label(self) -> &'static str {
        match self {
            Self::Pack => "pack",
            Self::Log => "log",
            Self::Checkpoint => "checkpoint",
            Self::Cache => "cache",
        }
    }
}

/// Return the newest compaction/checkpoint cursors represented by manifest.
fn trigger(manifest: &Manifest) -> GcTrigger {
    GcTrigger {
        compact_seq: manifest
            .packs
            .iter()
            .filter(|pack| pack.tier > 0)
            .map(|pack| pack.seq)
            .max()
            .unwrap_or(0),
        checkpoint_seq: manifest
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.seq),
    }
}

async fn read_state(handle: &RepoHandle) -> Result<GcState, String> {
    read_state_from(handle.store().clone()).await
}

async fn read_state_from(store: gitcask_store::Prefixed) -> Result<GcState, String> {
    match store.get_bytes(keys::GC).await {
        Ok(Some((_, bytes))) => GcState::decode(bytes.as_ref()).map_err(|error| error.to_string()),
        Ok(None) => Ok(GcState::default()),
        Err(error) => Err(error.to_string()),
    }
}

/// GC is event-driven: a pending repository is listed only after a compaction
/// or checkpoint newer than the durable gc.pb cursor.
pub async fn due(handle: &RepoHandle) -> Result<Option<GcTrigger>, String> {
    let current = trigger(&handle.manifest());
    if current == GcTrigger::default() {
        return Ok(None);
    }
    let state = read_state(handle).await?;
    Ok(
        (current.compact_seq > state.compact_seq || current.checkpoint_seq > state.checkpoint_seq)
            .then_some(current),
    )
}

/// Collect superseded pack families, folded log objects, and old checkpoint
/// directories under the per-repository gc lease.
pub async fn collect(
    handle: Arc<RepoHandle>,
    retention: Duration,
    shared_retention: Duration,
    lease_ttl: Duration,
) -> Result<GcOutcome, String> {
    let lease_store: gitcask_store::DynStore = Arc::new(handle.store().clone());
    let lease = gitcask_store::coord::try_acquire(
        lease_store,
        &keys::lease_key("gc"),
        gitcask_store::coord::instance_id(),
        "gc",
        lease_ttl,
    )
    .await
    .map_err(|error| error.to_string())?;
    let Some(lease) = lease else {
        return Err("gc lease held by another instance".into());
    };

    let result = collect_under_lease(handle.clone(), retention, shared_retention).await;
    if let Err(error) = lease.release().await {
        tracing::warn!(repo = %handle.id(), %error, "gc lease release failed");
    }
    result
}

async fn collect_under_lease(
    handle: Arc<RepoHandle>,
    retention: Duration,
    shared_retention: Duration,
) -> Result<GcOutcome, String> {
    let store = handle.store().clone();
    let repo = handle.id().to_string();
    let ((manifest_version, manifest), state) =
        tokio::try_join!(load_manifest(store.clone()), read_state_from(store.clone()))?;
    let observed = trigger(&manifest);
    let revision = manifest.revision;
    if observed.compact_seq <= state.compact_seq && observed.checkpoint_seq <= state.checkpoint_seq
    {
        return Ok(GcOutcome {
            trigger: observed,
            ..GcOutcome::default()
        });
    }

    tracing::info!(%repo, revision, trigger = %observed, "gc lease acquired; calculating references");
    let plan = plan(store.clone(), manifest, retention, shared_retention).await?;
    tracing::info!(%repo, packs = plan.packs.len(), logs = plan.logs.len(), checkpoints = plan.checkpoints.len(), caches = plan.caches.len(), "gc plan calculated");

    // A push or compaction that committed while LIST/GET planning ran must be
    // included in a newly calculated reference set. In particular, never use
    // an old manifest to delete a pack introduced by a newer generation.
    let (current_version, _) = load_manifest(store.clone()).await?;
    if current_version != manifest_version {
        return Err(
            "manifest changed while gc calculated references; retrying from a fresh generation"
                .into(),
        );
    }

    let deleted = delete_plan(store.clone(), plan).await?;
    let outcome = GcOutcome {
        trigger: observed,
        packs: deleted.packs,
        logs: deleted.logs,
        checkpoints: deleted.checkpoints,
        caches: deleted.caches,
        objects: deleted.objects,
    };

    let state = GcState {
        compact_seq: observed.compact_seq,
        checkpoint_seq: observed.checkpoint_seq,
        at: Some(gitcask_proto::time::now()),
    };
    store
        .put(
            keys::GC,
            PutBody::Bytes(state.encode_to_vec().into()),
            PutOptions::from(PutMode::Overwrite),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(outcome)
}

async fn load_manifest(store: gitcask_store::Prefixed) -> Result<(Version, Manifest), String> {
    let Some((meta, bytes)) = store
        .get_bytes(keys::MANIFEST)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Err("repository manifest disappeared during gc".into());
    };
    let manifest = Manifest::decode(bytes.as_ref()).map_err(|error| error.to_string())?;
    Ok((meta.version, manifest))
}

async fn plan(
    store: gitcask_store::Prefixed,
    manifest: Manifest,
    retention: Duration,
    shared_retention: Duration,
) -> Result<GcPlan, String> {
    let now = SystemTime::now();
    let mut objects = Vec::new();
    let mut listing = store.list("", None);
    while let Some(meta) = listing.next().await {
        objects.push(meta.map_err(|error| error.to_string())?);
    }

    // Shared entries are immutable derivations, so deleting one cannot affect
    // repository consistency: a later request simply computes and stores it
    // again. Their shorter retention is therefore independent of WAL data
    // retention and uses the LastModified values from this same repo listing.
    let mut old_caches: Vec<_> = objects
        .iter()
        .filter(|meta| expired_shared_cache(meta, now, shared_retention))
        .cloned()
        .collect();

    let mut referenced_packs: HashSet<String> = manifest
        .packs
        .iter()
        .map(|pack| pack.checksum.clone())
        .collect();
    let live_logs: HashSet<String> = manifest
        .log_segments
        .iter()
        .map(|segment| segment.key.clone())
        .collect();
    let mut superseded_at: HashMap<String, SystemTime> = HashMap::new();
    let mut old_logs = Vec::new();

    let log_metas: Vec<_> = objects
        .iter()
        .filter(|meta| meta.key.starts_with(keys::LOG_DIR))
        .cloned()
        .collect();
    let decoded_logs: Vec<_> = futures::stream::iter(log_metas)
        .map(|meta| {
            let store = store.clone();
            async move {
                let (_, bytes) = store
                    .get_bytes(&meta.key)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        format!("log object {} disappeared while gc planned", meta.key)
                    })?;
                let (entries, _) = gitcask_proto::frame::decode_entries(bytes.as_ref())
                    .map_err(|error| format!("decoding {}: {error}", meta.key))?;
                Ok::<_, String>((meta, entries))
            }
        })
        .buffer_unordered(32)
        .try_collect()
        .await?;
    for (meta, entries) in decoded_logs {
        let mut keep_log = live_logs.contains(&meta.key);
        for entry in &entries {
            let retained = within_retention(entry, now, retention);
            keep_log |= retained;
            if retained {
                if let Some(pack) = &entry.pack {
                    referenced_packs.insert(pack.checksum.clone());
                }
                referenced_packs.extend(entry.supersedes.iter().cloned());
            }
            if entry.kind == EntryKind::Compact as i32
                && let Some(created_at) = entry.created_at.as_ref()
            {
                let at = gitcask_proto::time::to_system(created_at);
                for checksum in &entry.supersedes {
                    superseded_at
                        .entry(checksum.clone())
                        .and_modify(|known| *known = (*known).max(at))
                        .or_insert(at);
                }
            }
        }
        if !keep_log {
            old_logs.push(meta);
        }
    }

    let current_checkpoint = manifest
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.key.clone());
    let mut checkpoint_dirs: HashMap<String, Vec<ObjectMeta>> = HashMap::new();
    for meta in objects
        .iter()
        .filter(|meta| meta.key.starts_with(keys::CHECKPOINTS_DIR))
    {
        if let Some((dir, _)) = meta.key.rsplit_once('/') {
            checkpoint_dirs
                .entry(format!("{dir}/"))
                .or_default()
                .push(meta.clone());
        }
    }
    let checkpoint_groups: Vec<_> = checkpoint_dirs.into_values().collect();
    let decoded_checkpoints: Vec<_> = futures::stream::iter(checkpoint_groups)
        .map(|mut group| {
            let store = store.clone();
            async move {
                group.sort_by(|left, right| left.key.cmp(&right.key));
                let Some(key) = group
                    .iter()
                    .find(|meta| meta.key.ends_with("/checkpoint.pb"))
                    .map(|meta| meta.key.clone())
                else {
                    return Ok::<_, String>(None);
                };
                let (_, bytes) = store
                    .get_bytes(&key)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("checkpoint {key} disappeared while gc planned"))?;
                let checkpoint = Checkpoint::decode(bytes.as_ref())
                    .map_err(|error| format!("decoding {key}: {error}"))?;
                Ok(Some((group, checkpoint)))
            }
        })
        .buffer_unordered(32)
        .try_collect()
        .await?;
    let mut old_checkpoints = Vec::new();
    for (group, checkpoint) in decoded_checkpoints.into_iter().flatten() {
        let Some(meta) = group
            .iter()
            .find(|meta| meta.key.ends_with("/checkpoint.pb"))
        else {
            continue;
        };
        let keep = current_checkpoint.as_deref() == Some(meta.key.as_str())
            || retained_timestamp(checkpoint.created_at.as_ref(), now, retention);
        if keep {
            referenced_packs.extend(checkpoint.packs.iter().map(|pack| pack.checksum.clone()));
        } else {
            old_checkpoints.push(group);
        }
    }

    let mut wal_families: HashMap<String, Vec<ObjectMeta>> = HashMap::new();
    for meta in objects
        .iter()
        .filter(|meta| meta.key.starts_with(keys::WAL_DIR))
    {
        if let Some(checksum) = wal_checksum(&meta.key) {
            wal_families
                .entry(checksum.to_string())
                .or_default()
                .push(meta.clone());
        }
    }
    let mut old_packs = Vec::new();
    for (checksum, mut family) in wal_families {
        let collectable = superseded_at.get(&checksum).is_some_and(|at| {
            !retained_time(*at, now, retention) && !referenced_packs.contains(&checksum)
        });
        if collectable {
            family.sort_by(|left, right| left.key.cmp(&right.key));
            old_packs.push(family);
        }
    }
    old_packs.sort_by(|left, right| left[0].key.cmp(&right[0].key));
    old_logs.sort_by(|left, right| left.key.cmp(&right.key));
    old_checkpoints.sort_by(|left, right| left[0].key.cmp(&right[0].key));
    old_caches.sort_by(|left, right| left.key.cmp(&right.key));

    Ok(GcPlan {
        packs: old_packs,
        logs: old_logs,
        checkpoints: old_checkpoints,
        caches: old_caches,
    })
}

fn expired_shared_cache(meta: &ObjectMeta, now: SystemTime, retention: Duration) -> bool {
    (meta.key.starts_with(keys::API_CACHE_DIR) || meta.key.starts_with(keys::ARCHIVE_CACHE_DIR))
        && !retained_time(meta.last_modified, now, retention)
}

fn within_retention(entry: &LogEntry, now: SystemTime, retention: Duration) -> bool {
    retained_timestamp(entry.created_at.as_ref(), now, retention)
}

fn retained_timestamp(
    timestamp: Option<&prost_types::Timestamp>,
    now: SystemTime,
    retention: Duration,
) -> bool {
    timestamp.is_none_or(|timestamp| {
        retained_time(gitcask_proto::time::to_system(timestamp), now, retention)
    })
}

fn retained_time(at: SystemTime, now: SystemTime, retention: Duration) -> bool {
    now.duration_since(at).map_or(true, |age| age < retention)
}

fn wal_checksum(key: &str) -> Option<&str> {
    let name = key.strip_prefix(keys::WAL_DIR)?;
    [".commit-graph", ".bitmap", ".pack", ".idx", ".rev"]
        .into_iter()
        .find_map(|suffix| name.strip_suffix(suffix))
}

async fn delete_group(
    store: gitcask_store::Prefixed,
    objects: Vec<ObjectMeta>,
) -> Result<usize, String> {
    let deleted: Vec<_> = futures::future::try_join_all(objects.into_iter().map(|meta| {
        let store = store.clone();
        async move {
            match store.delete(&meta.key, Some(meta.version)).await {
                Ok(()) => Ok(1),
                Err(StoreError::NotFound { .. }) => Ok(0),
                Err(error) => Err(format!("conditional delete {}: {error}", meta.key)),
            }
        }
    }))
    .await?;
    Ok(deleted.into_iter().sum())
}

async fn delete_plan(store: gitcask_store::Prefixed, plan: GcPlan) -> Result<DeleteCounts, String> {
    let GcPlan {
        packs,
        logs,
        checkpoints,
        caches,
    } = plan;
    let groups = packs
        .into_iter()
        .map(|group| (GcKind::Pack, group))
        .chain(logs.into_iter().map(|meta| (GcKind::Log, vec![meta])))
        .chain(
            checkpoints
                .into_iter()
                .map(|group| (GcKind::Checkpoint, group)),
        );
    let mut deleted: Vec<_> = futures::stream::iter(groups)
        .map(|(kind, group)| {
            let store = store.clone();
            async move {
                delete_group(store, group)
                    .await
                    .map(|objects| (kind, objects))
            }
        })
        .buffer_unordered(32)
        .try_collect()
        .await?;
    // Cache expiry is the lowest-priority part of the unit. It deliberately
    // reuses the listing above and starts only after WAL-derived collection.
    deleted.extend(
        futures::stream::iter(caches.into_iter().map(|meta| (GcKind::Cache, vec![meta])))
            .map(|(kind, group)| {
                let store = store.clone();
                async move {
                    delete_group(store, group)
                        .await
                        .map(|objects| (kind, objects))
                }
            })
            .buffer_unordered(32)
            .try_collect::<Vec<_>>()
            .await?,
    );
    let mut counts = DeleteCounts::default();
    for (kind, objects) in deleted {
        if objects == 0 {
            continue;
        }
        metrics::counter!("gitcask_gc_deleted_total", "kind" => kind.label()).increment(1);
        counts.objects += objects;
        match kind {
            GcKind::Pack => counts.packs += 1,
            GcKind::Log => counts.logs += 1,
            GcKind::Checkpoint => counts.checkpoints += 1,
            GcKind::Cache => counts.caches += objects,
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_every_pack_side_file() {
        for suffix in ["pack", "idx", "rev", "bitmap", "commit-graph"] {
            assert_eq!(wal_checksum(&format!("wal/abc.{suffix}")), Some("abc"));
        }
        assert_eq!(wal_checksum("wal/multi.part.pack"), Some("multi.part"));
        assert_eq!(wal_checksum("other/abc.pack"), None);
    }

    #[test]
    fn zero_retention_expires_past_timestamps() {
        let before = SystemTime::now() - Duration::from_secs(1);
        assert!(!retained_time(before, SystemTime::now(), Duration::ZERO));
    }

    #[test]
    fn only_old_shared_cache_objects_expire() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let old = now - Duration::from_secs(31);
        let current = now - Duration::from_secs(29);
        let meta = |key: &str, last_modified| ObjectMeta {
            key: key.to_string(),
            size: 1,
            version: Version::new("test"),
            last_modified,
        };

        assert!(expired_shared_cache(
            &meta("cache/api/v1/old.json", old),
            now,
            Duration::from_secs(30)
        ));
        assert!(expired_shared_cache(
            &meta("cache/archive/v1/old.tar.gz", old),
            now,
            Duration::from_secs(30)
        ));
        assert!(!expired_shared_cache(
            &meta("cache/api/v1/current.json", current),
            now,
            Duration::from_secs(30)
        ));
        assert!(!expired_shared_cache(
            &meta("wal/not-a-cache.pack", old),
            now,
            Duration::from_secs(30)
        ));
    }
}
