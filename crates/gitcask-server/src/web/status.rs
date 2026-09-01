use std::sync::Arc;

use axum::Router;
use std::fs;
use std::path::Path;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use gitcask_proto::v1::{Checkpoint, EntryKind};
use gitcask_store::{GetOptions, GetResult, ObjectStore};
use prost::Message;
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;
use crate::error::ApiError;

/// Repository status/ops JSON: `overview`, `ops`, `tasks` under every API base.
pub fn router(state: Arc<AppState>) -> Router {
    let mut r = Router::new();
    for base in crate::web::api::REPO_API_BASES {
        r = r
            .route(&format!("{base}/overview"), get(overview))
            .route(&format!("{base}/ops"), get(ops_list))
            .route(
                &format!("{base}/ops/{{op}}"),
                axum::routing::post(ops_start),
            )
            .route(&format!("{base}/tasks"), get(tasks_list))
            .route(&format!("{base}/tasks/{{id}}"), get(task_stream));
    }
    r.with_state(state)
}

#[derive(Serialize, ToSchema)]
struct Overview {
    repo: String,
    pending: bool,
    clone_url: String,
    hostname: String,
    health: Health,
    manifest: ManifestInfo,
    local: LocalInfo,
    packs: PacksInfo,
    compactions: Vec<CompactionInfo>,
    #[schema(value_type = Object)]
    node: serde_json::Map<String, serde_json::Value>,
    ops: OpsInfo,
    /// Ready-to-paste git invocations for this repo.
    clone: CloneInfo,
}

#[derive(Serialize, ToSchema)]
struct CloneInfo {
    /// Plain clone through the front proxy.
    plain: String,
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: &'static str,
    issues: Vec<String>,
    /// The last connectivity audit as the maintainer recorded it in `fsck.pb` (any host), else
    /// "never audited".
    deep: String,
    /// Maintenance this repository is missing; each maps to an op. `auto` says when the
    /// maintainer loop does it by itself — then the button is a "do it now", not a chore.
    suggestions: Vec<Suggestion>,
}

#[derive(Serialize, ToSchema)]
struct Suggestion {
    op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<String>,
    reason: String,
    /// How/when the maintainer loop performs this without anyone asking (None: a human must).
    #[serde(skip_serializing_if = "Option::is_none")]
    auto: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct OpsInfo {
    available: Vec<crate::ops::OpSpec>,
    /// Recent + running tasks on this instance (ops and materialization).
    recent: Vec<gitcask_wal::TaskRecord>,
}

#[derive(Serialize, ToSchema)]
struct ManifestInfo {
    version: String,
    next_seq: u64,
    min_seq: u64,
    segments: Vec<SegmentInfo>,
    tail_entries: usize,
    entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<CheckpointInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packset: Option<PacksetInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_push: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct SegmentInfo {
    key: String,
    first_seq: u64,
    last_seq: u64,
    size: u64,
}

#[derive(Serialize, ToSchema)]
struct PacksetInfo {
    at_seq: u64,
    packs: usize,
    bytes: u64,
    created: String,
    creator: String,
}

#[derive(Serialize, ToSchema)]
struct CheckpointInfo {
    key: String,
    size: u64,
    at_seq: u64,
    created: String,
    creator: String,
}

#[derive(Serialize, ToSchema)]
struct LocalInfo {
    version: String,
    next_seq: u64,
    bootstrap: u64,
    reconciled: bool,
    size_bytes: u64,
    /// How objects are served here: `local` (packs on disk) or `pending`
    /// (packs not yet downloaded).
    objects: &'static str,
}

#[derive(Serialize, ToSchema)]
struct PacksInfo {
    live: usize,
    live_bytes: u64,
    pushes: usize,
}

#[derive(Serialize, ToSchema)]
struct CompactionInfo {
    seq: u64,
    level: u32,
    first_seq: u64,
    last_seq: u64,
    pack_size: u64,
    superseded_packs: usize,
    superseded_bytes: u64,
    at: String,
    primary: String,
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/overview",
    tag = "operations",
    summary = "Get repository operational status",
    description = "Returns WAL, checkpoint, pack, local-cache, health, and maintenance information for one repository.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 200, description = "Repository overview", body = Overview),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn overview(
    State(state): State<Arc<AppState>>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.auth.require_read(&headers, &owner, &repo).await?;
    let id =
        gitcask_git::RepoId::new(&owner, &repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
    let pending_key = gitcask_proto::keys::pending_key(id.owner(), id.name());
    let (pending, handle) = tokio::join!(state.store.head(&pending_key), state.registry.open(&id),);
    let pending = pending?.is_some();
    let handle = handle?;
    // read_log performs its own freshness check; acquire the read guard only
    // after it has completed because read_log may need the write lock.
    let entries = handle.read_log(1, None).await?;
    // Refs-level sync only: the overview must render for repos whose packs do
    // not fit this instance (that is exactly when people look at it).
    let _guard = handle.sync_refs().await?;
    let manifest = handle.manifest();
    let version = handle
        .manifest_version()
        .map(|version| version.to_string())
        .unwrap_or_default();
    let base_url = crate::smart::request_base_url(&state, &headers);
    let clone_url = format!("{base_url}/{id}.git");

    let checkpoint = checkpoint_info(&handle, &manifest).await?;
    let created = manifest
        .updated_at
        .as_ref()
        .map(timestamp)
        .unwrap_or_default();
    let packs_bytes = manifest.packs.iter().map(|pack| pack.pack_size).sum();
    let packset = if manifest.packs.is_empty() {
        None
    } else {
        Some(PacksetInfo {
            at_seq: manifest.head_seq,
            packs: manifest.packs.len(),
            bytes: packs_bytes,
            created: created.clone(),
            creator: manifest.writer.clone(),
        })
    };
    let last_push = entries
        .iter()
        .filter(|entry| entry.kind() == EntryKind::Push)
        .filter_map(|entry| entry.created_at.as_ref().map(timestamp))
        .last();
    let mut push_count = 0;
    let mut compactions = Vec::new();
    let mut pack_by_checksum = std::collections::HashMap::new();
    for entry in &entries {
        if let Some(pack) = &entry.pack {
            pack_by_checksum.insert(pack.checksum.as_str(), (pack.seq, pack.pack_size));
        }
        if entry.kind() == EntryKind::Push && entry.pack.is_some() {
            push_count += 1;
        }
        if entry.kind() == EntryKind::Compact {
            let mut first = u64::MAX;
            let mut last = 0;
            let mut bytes = 0;
            for checksum in &entry.supersedes {
                if let Some((seq, size)) = pack_by_checksum.get(checksum.as_str()) {
                    first = first.min(*seq);
                    last = last.max(*seq);
                    bytes += *size;
                }
            }
            compactions.push(CompactionInfo {
                seq: entry.seq,
                level: entry.pack.as_ref().map_or(0, |pack| pack.tier),
                first_seq: if first == u64::MAX { 0 } else { first },
                last_seq: last,
                pack_size: entry.pack.as_ref().map_or(0, |pack| pack.pack_size),
                superseded_packs: entry.supersedes.len(),
                superseded_bytes: bytes,
                at: entry.created_at.as_ref().map(timestamp).unwrap_or_default(),
                primary: entry.writer.clone(),
            });
        }
    }
    let size_bytes = repo_size(handle.local().path()).await;
    let local_version = handle.local_version().unwrap_or_default();
    let reconciled = local_version == version && handle.applied_seq() == manifest.head_seq;
    let objects_mode = if handle.packs_ready() {
        "local"
    } else {
        "pending"
    };
    // Health + suggestions.
    let mut issues = Vec::new();
    let mut suggestions = Vec::new();
    let storage_note = format!(
        "local packs · {} on {} (eviction above {:.0}% disk use)",
        bytesize::ByteSize::b(
            manifest
                .packs
                .iter()
                .map(|p| p.pack_size + p.idx_size)
                .sum()
        ),
        state.cfg.cache.dir.display(),
        state.cfg.cache.disk_high_watermark * 100.0
    );
    if manifest.head_seq > 0 && !reconciled {
        issues.push(format!(
            "local copy on {} is at seq {} but the WAL head is {}",
            gitcask_store::coord::instance_id(),
            handle.applied_seq(),
            manifest.head_seq
        ));
        suggestions.push(Suggestion {
            op: "sync",
            params: None,
            reason: "catch the local copy up to the WAL head".into(),
            auto: Some(
                "the next request to this instance revalidates (one conditional GET)".into(),
            ),
        });
    }
    let fresh = manifest.packs.iter().filter(|p| p.tier == 0).count();
    let ecfg = state.cfg.clone();
    let compaction_on =
        ecfg.compaction.enabled && state.cfg.has_role(gitcask_config::Role::Compact);
    if fresh >= ecfg.compaction.trigger_packs.max(2) {
        suggestions.push(Suggestion {
            op: "compact",
            params: None,
            reason: format!("{fresh} fresh push packs waiting to be folded"),
            auto: compaction_on.then(|| {
                format!(
                    "geometric fold on the maintainer's next pass (trigger: {} packs / {})",
                    ecfg.compaction.trigger_packs, ecfg.compaction.trigger_bytes
                )
            }),
        });
    }
    if manifest.head_seq > 0 {
        let cp_seq = manifest.checkpoint.as_ref().map(|c| c.seq).unwrap_or(0);
        let behind = manifest.head_seq.saturating_sub(cp_seq);
        if behind >= state.cfg.wal.snapshot_every_entries.max(1) || (cp_seq == 0 && behind > 0) {
            suggestions.push(Suggestion {
                op: "checkpoint",
                params: None,
                reason: if cp_seq == 0 {
                    "no checkpoint yet: cold materialize replays the whole log".into()
                } else {
                    format!("checkpoint is {behind} entries behind the head")
                },
                auto: Some(format!(
                    "first unit of the next pending-marker pass (every {} entries / {} / {} of tail)",
                    state.cfg.wal.snapshot_every_entries,
                    humantime::format_duration(state.cfg.wal.checkpoint_interval),
                    state.cfg.wal.checkpoint_tail_bytes
                )),
            });
        }
    }
    // The audit verdict lives in the store (`fsck.pb`, written by whichever maintainer ran it),
    // not in this instance's task memory.
    let fsck_report = crate::ops::read_fsck(&handle).await.ok().flatten();
    let deep = match &fsck_report {
        Some(r) => {
            let when =
                r.at.as_ref()
                    .map(|t| t.seconds)
                    .map(|s| {
                        chrono::DateTime::from_timestamp(s, 0)
                            .map(|d| d.format("%Y-%m-%d %H:%MZ").to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
            let verdict = if r.missing_total == 0 && r.problems == 0 {
                "clean".to_string()
            } else {
                format!(
                    "{} missing object(s), {} other problem(s)",
                    r.missing_total, r.problems
                )
            };
            format!(
                "{verdict} at seq {} ({when}, {}, {:.1}s)",
                r.seq, r.host, r.elapsed_secs
            )
        }
        None => "never audited".into(),
    };
    if fsck_report.is_none() && manifest.head_seq > 0 {
        let compacted = manifest.packs.iter().any(|pack| pack.tier > 0);
        suggestions.push(Suggestion {
            op: "fsck",
            params: Some("connectivity=1".into()),
            reason: "connectivity never audited".into(),
            auto: compacted.then(|| {
                "lowest-priority unit of the next pending-marker pass after compaction".into()
            }),
        });
    } else if let Some(r) = &fsck_report
        && (r.missing_total > 0 || r.problems > 0)
    {
        issues.push(format!(
            "last fsck found {} missing object(s), {} other problem(s)",
            r.missing_total, r.problems
        ));
    }
    let status = if issues.iter().any(|i| i.starts_with("last fsck found")) {
        "error"
    } else if !issues.is_empty() {
        "degraded"
    } else {
        "ok"
    };
    let ops = OpsInfo {
        available: crate::ops::OPS.to_vec(),
        recent: state.registry.tasks().recent(&id.to_string()),
    };
    let clone = CloneInfo {
        plain: format!("git clone {clone_url}"),
    };
    let body = Overview {
        repo: id.to_string(),
        pending,
        clone_url,
        hostname: gitcask_store::coord::instance_id().to_string(),
        health: Health {
            status,
            issues,
            deep,
            suggestions,
        },
        manifest: ManifestInfo {
            version: version.clone(),
            next_seq: manifest.head_seq.saturating_add(1),
            min_seq: manifest.min_seq,
            segments: manifest
                .log_segments
                .iter()
                .map(|segment| SegmentInfo {
                    key: segment.key.clone(),
                    first_seq: segment.first_seq,
                    last_seq: segment.last_seq,
                    size: segment.size,
                })
                .collect(),
            tail_entries: entries.len(),
            entries: entries.len(),
            checkpoint,
            packset,
            last_push,
        },
        local: LocalInfo {
            version: local_version.clone(),
            next_seq: handle.applied_seq().saturating_add(1),
            bootstrap: handle.applied_seq(),
            reconciled,
            size_bytes,
            objects: objects_mode,
        },
        packs: PacksInfo {
            live: manifest.packs.len(),
            live_bytes: packs_bytes,
            pushes: push_count,
        },
        compactions,
        node: {
            let mut m = serde_json::Map::new();
            m.insert("storage".into(), serde_json::Value::String(storage_note));
            m
        },
        ops,
        clone,
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::to_vec(&body).map_err(|e| ApiError::Internal(e.to_string()))?,
    )
        .into_response())
}

/// `GET …/ops` — available ops + recent outcomes on this instance.
#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/ops",
    tag = "operations",
    summary = "List repository operations",
    description = "Lists available maintenance operations and recent task outcomes on this instance.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 200, description = "Operations and recent tasks", body = OpsInfo),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn ops_list(
    State(state): State<Arc<AppState>>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.auth.require_read(&headers, &owner, &repo).await?;
    let id =
        gitcask_git::RepoId::new(&owner, &repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
    let body = OpsInfo {
        available: crate::ops::OPS.to_vec(),
        recent: state.registry.tasks().recent(&id.to_string()),
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::to_vec(&body).map_err(|e| ApiError::Internal(e.to_string()))?,
    )
        .into_response())
}

/// `POST …/ops/{op}?<params>` — run a maintenance op on this instance as a
/// background task and stream it (SSE envelope: `task`, `notice`, `progress`,
/// then `result` `{"task","value"}` or `error`). Write permission required.
/// If the same op is already running here the response attaches to that task
/// instead (same stream shape; its `task.id` tells you which).
async fn ops_start(
    State(state): State<Arc<AppState>>,
    AxumPath((owner, repo, op)): AxumPath<(String, String, String)>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let principal = state.auth.require_write(&headers, &owner, &repo).await?;
    let id =
        gitcask_git::RepoId::new(&owner, &repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
    // Make sure the repo exists before spawning anything.
    state.registry.open(&id).await?;
    tracing::info!(repo = %id, op = %op, by = %principal.name, ?params, "ops.start");
    let task = match crate::ops::start(state.clone(), id, &op, params).await {
        Ok(t) => t,
        Err(crate::ops::StartError::UnknownOp) => {
            return Err(ApiError::NotFound(format!("unknown op {op}")));
        }
        Err(crate::ops::StartError::AlreadyRunning(existing)) => existing,
    };
    Ok(crate::sse::task_stream(task))
}

/// `GET …/tasks` — running + recent background tasks of this repo on this
/// instance (materialize, fsck, compact, ...). The UI
/// polls this to show what is happening to a repo.
#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/tasks",
    tag = "operations",
    summary = "List repository tasks",
    description = "Returns running and recent background tasks for this repository on the current instance.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 200, description = "Running and recent tasks", body = crate::web::openapi::TasksResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn tasks_list(
    State(state): State<Arc<AppState>>,
    AxumPath((owner, repo)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.auth.require_read(&headers, &owner, &repo).await?;
    let id =
        gitcask_git::RepoId::new(&owner, &repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
    let tasks = state.registry.tasks();
    let body = serde_json::json!({
        "hostname": gitcask_store::coord::instance_id(),
        "running": tasks.running(&id.to_string()),
        "recent": tasks.recent(&id.to_string()),
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        serde_json::to_vec(&body).map_err(|e| ApiError::Internal(e.to_string()))?,
    )
        .into_response())
}

/// `GET …/tasks/{id}` — attach to a task: SSE replay of its packets so far,
/// then live, then the terminal `result`/`error`. JSON (no SSE accept) returns
/// the record.
#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/tasks/{id}",
    tag = "operations",
    summary = "Get or attach to a task",
    description = "Returns the task record as JSON, or a replayable live stream when the client accepts `text/event-stream`.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("id" = String, Path, description = "Per-instance task id")
    ),
    responses(
        (status = 200, description = "Task record or SSE stream", body = gitcask_wal::TaskRecord),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or task not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn task_stream(
    State(state): State<Arc<AppState>>,
    AxumPath((owner, repo, task_id)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.auth.require_read(&headers, &owner, &repo).await?;
    let id =
        gitcask_git::RepoId::new(&owner, &repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
    let task = state
        .registry
        .tasks()
        .get(&task_id)
        .filter(|t| t.record().repo == id.to_string())
        .ok_or_else(|| ApiError::NotFound(format!("task {task_id} (tasks are per instance; this one may have run elsewhere or aged out)")))?;
    if !crate::sse::wants_sse(&headers) {
        return Ok((
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            serde_json::to_vec(&task.record()).map_err(|e| ApiError::Internal(e.to_string()))?,
        )
            .into_response());
    }
    Ok(crate::sse::task_stream(task))
}

async fn checkpoint_info(
    handle: &gitcask_wal::RepoHandle,
    manifest: &gitcask_proto::v1::Manifest,
) -> Result<Option<CheckpointInfo>, ApiError> {
    let Some(reference) = &manifest.checkpoint else {
        return Ok(None);
    };
    let (size, created, creator) = match handle
        .store()
        .get(&reference.key, GetOptions::default())
        .await
    {
        Ok(GetResult::Object { meta, body }) => {
            let bytes = gitcask_store::util::collect(body, meta.size as usize)
                .await
                .map_err(ApiError::from)?;
            let checkpoint = Checkpoint::decode(bytes.as_ref())
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            (
                meta.size,
                checkpoint
                    .created_at
                    .as_ref()
                    .map(timestamp)
                    .unwrap_or_default(),
                checkpoint.writer,
            )
        }
        Ok(GetResult::NotModified { .. }) => (0, String::new(), String::new()),
        Err(gitcask_store::StoreError::NotFound { .. }) => (0, String::new(), String::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(CheckpointInfo {
        key: reference.key.clone(),
        size,
        at_seq: reference.seq,
        created,
        creator,
    }))
}

fn timestamp(value: &prost_types::Timestamp) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(value.seconds, value.nanos as u32)
        .map(|date| date.to_rfc3339())
        .unwrap_or_default()
}

async fn repo_size(path: &Path) -> u64 {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || size_recursive(&path))
        .await
        .unwrap_or(0)
}

fn size_recursive(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| size_recursive(&entry.path()))
        .sum()
}
