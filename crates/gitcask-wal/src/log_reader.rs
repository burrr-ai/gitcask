//! Read log entries from the store (provenance/rewind tooling).

use gitcask_proto::v1::LogEntry;
use gitcask_store::{GetOptions, GetResult, ObjectStore};

use crate::error::WalError;
use crate::handle::RepoHandle;

/// Read log entries in [from_seq, to_seq]. If `to_seq` is None, read up to
/// `manifest.head_seq`.
pub(crate) async fn read_log_impl(
    handle: &RepoHandle,
    from_seq: u64,
    to_seq: Option<u64>,
) -> Result<Vec<LogEntry>, WalError> {
    // Reading the log needs a *fresh manifest*, not a synced local copy: do a
    // lock-free conditional GET and use whichever manifest is newer. Taking
    // the repo's write lock here would deadlock callers that hold a read
    // guard (overview, tests), and freshness_ttl=0 makes that the common case.
    let known = handle.manifest_version.lock().clone();
    let manifest = match crate::sync::freshness_check(&handle.store, &known).await? {
        crate::sync::SyncOutcome::Unchanged => handle.manifest.read().clone(),
        crate::sync::SyncOutcome::Changed { manifest, .. } => std::sync::Arc::new(manifest),
    };
    let head_seq = manifest.head_seq;
    let to = to_seq.unwrap_or(head_seq).min(head_seq);

    if from_seq > to {
        return Ok(Vec::new());
    }

    // Find relevant segments
    let segments: Vec<&gitcask_proto::v1::LogSegmentRef> = manifest
        .log_segments
        .iter()
        .filter(|s| s.last_seq >= from_seq && s.first_seq <= to)
        .collect();

    let mut entries = Vec::new();
    for seg in &segments {
        let res = handle.store.get(&seg.key, GetOptions::default()).await?;
        let bytes = match res {
            GetResult::Object { meta, body } => {
                gitcask_store::util::collect(body, meta.size as usize).await?
            }
            GetResult::NotModified { .. } => continue,
        };

        let (seg_entries, _) = gitcask_proto::frame::decode_entries(&bytes)
            .map_err(|e| WalError::Corrupt(format!("log segment decode: {e}")))?;

        for entry in seg_entries {
            if entry.seq >= from_seq && entry.seq <= to {
                entries.push(entry);
            }
        }
    }

    entries.sort_by_key(|e| e.seq);
    Ok(entries)
}
