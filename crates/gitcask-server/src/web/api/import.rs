//! Full-history pristine imports: immutable pin -> bulk acquisition -> WAL CAS.
use super::{
    import_transport::{self as transport, Relay},
    write::{mutation_meta, open_write},
};
use crate::{AppState, error::ApiError, sse};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use gitcask_git::{IngestOptions, LocalRepo, RepoId};
use gitcask_proto::v1::{ImportReceipt, RefTransaction, RefUpdate};
use gitcask_wal::{Begin, RepoHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, process::Stdio, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use utoipa::ToSchema;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportRef {
    name: String,
    oid: String,
    peeled: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportHead {
    symbolic_target: String,
    oid: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportSnapshot {
    source: String,
    object_format: String,
    refs: Vec<ImportRef>,
    head: ImportHead,
    snapshot_hash: String,
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveImportRequest {
    source: String,
}
#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportRequest {
    operation_key: String,
    snapshot: ImportSnapshot,
}
#[derive(Serialize, ToSchema)]
pub(crate) struct ImportResult {
    operation_key: String,
    request_hash: String,
    snapshot_hash: String,
    seq: u64,
    refs_count: u64,
    head: ImportHead,
    replayed: bool,
}
impl ImportResult {
    fn from_receipt(r: &ImportReceipt, replayed: bool) -> Self {
        Self {
            operation_key: r.operation_key.clone(),
            request_hash: r.request_hash.clone(),
            snapshot_hash: r.snapshot_hash.clone(),
            seq: r.seq,
            refs_count: r.refs_count,
            head: ImportHead {
                symbolic_target: r.head_target.clone(),
                oid: r.head_oid.clone(),
            },
            replayed,
        }
    }
    fn response(self) -> Response {
        (
            if self.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            [("cache-control", "no-store")],
            Json(self),
        )
            .into_response()
    }
}

fn json_error(error: &axum::extract::rejection::JsonRejection) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::PayloadTooLarge
    } else {
        ApiError::BadRequest(error.body_text())
    }
}

fn source_id(source: &str) -> Result<Option<RepoId>, ApiError> {
    if source.starts_with("https://") {
        transport::validate_url(source)?;
        return Ok(None);
    }
    let (owner, repo) = source.split_once('/').ok_or_else(|| {
        ApiError::BadRequest("source must be owner/repo or public HTTPS URL".into())
    })?;
    Ok(Some(RepoId::new(owner, repo).map_err(|_| {
        ApiError::BadRequest("invalid source repository".into())
    })?))
}
fn operation_key(key: &str) -> Result<(), ApiError> {
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ApiError::BadRequest("invalid operation_key".into()));
    }
    Ok(())
}
fn hash_fields(prefix: &[u8], fields: impl IntoIterator<Item = String>) -> String {
    let mut hash = Sha256::new();
    hash.update(prefix);
    for field in fields {
        hash.update(format!("{}:", field.len()).as_bytes());
        hash.update(field.as_bytes());
    }
    hex::encode(hash.finalize())
}
fn snapshot_hash(s: &ImportSnapshot) -> String {
    let mut fields = vec![
        s.source.clone(),
        s.object_format.clone(),
        s.head.symbolic_target.clone(),
        s.head.oid.clone(),
        s.refs.len().to_string(),
    ];
    for r in &s.refs {
        fields.extend([r.name.clone(), r.oid.clone(), r.peeled.clone()]);
    }
    hash_fields(b"gitcask-import-snapshot-v1\n", fields)
}
fn request_hash(owner: &str, repo: &str, request: &ImportRequest) -> String {
    hash_fields(
        b"gitcask-import-request-v1\n",
        [
            owner.to_string(),
            repo.to_string(),
            request.operation_key.clone(),
            request.snapshot.snapshot_hash.clone(),
        ],
    )
}
fn validate_snapshot(s: &ImportSnapshot, max_refs: usize) -> Result<(), ApiError> {
    source_id(&s.source)?;
    if serde_json::to_vec(s)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .len()
        > 1024 * 1024
    {
        return Err(ApiError::PayloadTooLarge);
    }
    let length = match s.object_format.as_str() {
        "sha1" => 40,
        "sha256" if !s.source.starts_with("https://") => 64,
        _ => {
            return Err(ApiError::BadRequest(
                "unsupported source object format".into(),
            ));
        }
    };
    if s.refs.len() > max_refs {
        return Err(ApiError::PayloadTooLarge);
    }
    if s.head.oid.is_empty() {
        return Err(ApiError::UnprocessableEntity(
            "empty or unborn sources are unsupported".into(),
        ));
    }
    let oid = |value: &str| -> Result<(), ApiError> {
        gitcask_git::validate_oid(value)?;
        if value.len() != length
            || value.bytes().all(|b| b == b'0')
            || value.bytes().any(|b| b.is_ascii_uppercase())
        {
            return Err(ApiError::BadRequest(
                "expected canonical nonzero source oid".into(),
            ));
        }
        Ok(())
    };
    oid(&s.head.oid)?;
    let mut previous = "";
    for r in &s.refs {
        gitcask_git::validate_ref_name(&r.name)?;
        if !(r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/"))
            || r.name.bytes().any(|b| b <= 32 || b == 127)
            || r.name
                .split('/')
                .any(|part| part.starts_with('.') || part.ends_with(".lock"))
            || r.name.len() > 1024
            || r.name.as_str() <= previous
        {
            return Err(ApiError::BadRequest(
                "snapshot refs must be unique sorted heads/tags".into(),
            ));
        }
        oid(&r.oid)?;
        if !r.peeled.is_empty() {
            oid(&r.peeled)?;
        }
        previous = &r.name;
    }
    if !s.head.symbolic_target.is_empty()
        && !s.refs.iter().any(|r| {
            r.name == s.head.symbolic_target
                && r.oid == s.head.oid
                && r.name.starts_with("refs/heads/")
        })
    {
        return Err(ApiError::BadRequest(
            "symbolic HEAD must name a selected branch at the pinned oid".into(),
        ));
    }
    if s.snapshot_hash != snapshot_hash(s) {
        return Err(ApiError::BadRequest("snapshot_hash mismatch".into()));
    }
    Ok(())
}

#[utoipa::path(post, path="/{owner}/{repo}/api/import/resolve", tag="writes", summary="Pin a full-history import source", request_body=ResolveImportRequest,
 params(("owner"=String,Path),("repo"=String,Path)), responses((status=200,body=ImportSnapshot,description="Pinned sorted heads/tags and HEAD; persist before import"),(status=400,description="Invalid or unsafe source"),(status=422,description="Empty or unsupported source"),(status=503,description="Temporary source failure")), security(("jwt_bearer"=[])))]
pub(crate) async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    request: Result<Json<ResolveImportRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = request.map_err(|error| json_error(&error))?;
    if gitcask_wal::tasks::shutting_down() {
        return Err(ApiError::ImportUnavailable("server draining".into()));
    }
    let (destination, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let id = source_id(&request.source)?;
    if let Some(id) = &id {
        principal.require_read(id.owner(), id.name())?;
    }
    let progress = destination.subscribe_progress();
    let threads = state.cfg.cache.bulk_threads;
    let work = async move {
        gitcask_wal::on_bulk_runtime(threads, async move {
            Ok(run_resolve(state, destination, request.source, id).await)
        })
        .await?
    };
    if sse::wants_sse(&headers) {
        Ok(sse::envelope(vec![progress], async move {
            let snapshot = work.await?;
            Ok(sse::Rendered::json(
                serde_json::to_vec(&snapshot)
                    .map_err(|e| ApiError::Internal(e.to_string()))?
                    .into(),
                "no-store",
                None,
            ))
        }))
    } else {
        Ok(([("cache-control", "no-store")], Json(work.await?)).into_response())
    }
}
async fn run_resolve(
    state: Arc<AppState>,
    destination: Arc<RepoHandle>,
    source: String,
    id: Option<RepoId>,
) -> Result<ImportSnapshot, ApiError> {
    let task = match destination.begin_task("import_resolve", HashMap::new()) {
        Begin::Started(task) => task,
        Begin::AlreadyRunning(_) => {
            return Err(ApiError::ImportUnavailable(
                "source resolution already running; retry".into(),
            ));
        }
    };
    task.notice("Resolving source heads/tags and HEAD without acquiring history");
    let result = tokio::time::timeout(
        state.cfg.import.resolve_timeout,
        resolve_source(&state, source, id),
    )
    .await
    .unwrap_or_else(|_| {
        Err(ApiError::ImportUnavailable(
            "resolve deadline exceeded".into(),
        ))
    });
    match &result {
        Ok(snapshot) => {
            task.finish_ok(
                "Source snapshot resolved",
                // Target readers can inspect its task log before import CAS;
                // keep source refs/HEAD private to the authorized HTTP result.
                Some(serde_json::json!({"snapshot_hash":snapshot.snapshot_hash,"refs_count":snapshot.refs.len()})),
            );
        }
        Err(error) => {
            task.finish_err(error.status().as_u16(), error.message());
        }
    }
    result
}

async fn resolve_source(
    state: &AppState,
    source: String,
    id: Option<RepoId>,
) -> Result<ImportSnapshot, ApiError> {
    let mut snapshot = if let Some(id) = id {
        let handle = state.registry.open(&id).await?;
        let guard = handle.sync_refs_only().await?;
        let refs = handle.refs_snapshot().await?;
        drop(guard);
        let head_oid = if refs.head_target.is_empty() {
            refs.head_oid
        } else {
            refs.refs
                .iter()
                .find(|r| r.name == refs.head_target)
                .map(|r| r.oid.clone())
                .unwrap_or_default()
        };
        ImportSnapshot {
            source,
            object_format: handle.manifest().object_format.clone(),
            refs: refs
                .refs
                .into_iter()
                .filter(|r| r.name.starts_with("refs/heads/") || r.name.starts_with("refs/tags/"))
                .map(|r| ImportRef {
                    name: r.name,
                    oid: r.oid,
                    peeled: r.peeled,
                })
                .collect(),
            head: ImportHead {
                symbolic_target: refs.head_target,
                oid: head_oid,
            },
            snapshot_hash: String::new(),
        }
    } else {
        let relay = Relay::start(&source, 4 * 1024 * 1024).await?;
        let scratch = tempfile::tempdir().map_err(|e| ApiError::Internal(e.to_string()))?;
        let mut command = transport::git(scratch.path());
        command.args([
            "ls-remote",
            "--symref",
            &relay.url,
            "HEAD",
            "refs/heads/*",
            "refs/tags/*",
        ]);
        let listing = transport::output(command, 1024 * 1024)
            .await
            .map_err(|error| relay.error(error))?;
        parse_listing(source, &listing)?
    };
    snapshot.snapshot_hash = snapshot_hash(&snapshot);
    validate_snapshot(&snapshot, state.cfg.import.max_refs)?;
    Ok(snapshot)
}
fn parse_listing(source: String, listing: &[u8]) -> Result<ImportSnapshot, ApiError> {
    let text = std::str::from_utf8(listing)
        .map_err(|_| ApiError::BadRequest("invalid source advertisement".into()))?;
    let mut refs = std::collections::BTreeMap::<String, ImportRef>::new();
    let mut head = ImportHead {
        symbolic_target: String::new(),
        oid: String::new(),
    };
    let mut peeled = Vec::new();
    for line in text.lines() {
        let (value, name) = line
            .split_once('\t')
            .ok_or_else(|| ApiError::BadRequest("invalid source advertisement".into()))?;
        if name == "HEAD" {
            if let Some(target) = value.strip_prefix("ref: ") {
                head.symbolic_target = target.into();
            } else {
                head.oid = value.to_ascii_lowercase();
            }
        } else if let Some(name) = name.strip_suffix("^{}") {
            peeled.push((name.to_string(), value.to_ascii_lowercase()));
        } else if refs
            .insert(
                name.into(),
                ImportRef {
                    name: name.into(),
                    oid: value.to_ascii_lowercase(),
                    peeled: String::new(),
                },
            )
            .is_some()
        {
            return Err(ApiError::BadRequest("duplicate source ref".into()));
        }
    }
    for (name, oid) in peeled {
        if let Some(r) = refs.get_mut(&name) {
            r.peeled = oid;
        }
    }
    Ok(ImportSnapshot {
        source,
        object_format: "sha1".into(),
        refs: refs.into_values().collect(),
        head,
        snapshot_hash: String::new(),
    })
}

fn replay_or_pristine(
    handle: &RepoHandle,
    key: &str,
    hash: &str,
) -> Result<Option<ImportResult>, ApiError> {
    let manifest = handle.manifest();
    if let Some(r) = &manifest.import_receipt {
        if r.operation_key == key && r.request_hash == hash {
            return Ok(Some(ImportResult::from_receipt(r, true)));
        }
        return Err(ApiError::Conflict(
            "import operation key or fingerprint differs".into(),
        ));
    }
    if !gitcask_wal::is_pristine(&manifest) {
        return Err(ApiError::Conflict("repository is not pristine".into()));
    }
    Ok(None)
}

#[utoipa::path(post, path="/{owner}/{repo}/api/import", tag="writes", summary="Import pinned complete history into a pristine repository", description="Requires target write and internal source read; exact retries return the same-CAS receipt without opening source or moving later writes. LFS pointer blobs in any acquired history are unsupported; gitlinks are preserved without recursive fetch. SSE uses task/result/error envelope; Cloud owns durable jobs.", request_body=ImportRequest,
 params(("owner"=String,Path),("repo"=String,Path)), responses((status=201,body=ImportResult,description="History committed"),(status=200,body=ImportResult,description="Exact replay or SSE"),(status=409,description="Nonpristine, request conflict or pinned source unavailable"),(status=413,description="Import bounds exceeded"),(status=422,description="LFS unsupported"),(status=503,description="Retry identical request")), security(("jwt_bearer"=[])))]
pub(crate) async fn import(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    request: Result<Json<ImportRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = request.map_err(|error| json_error(&error))?;
    if gitcask_wal::tasks::shutting_down() {
        return Err(ApiError::ImportUnavailable("server draining".into()));
    }
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    operation_key(&request.operation_key)?;
    validate_snapshot(&request.snapshot, state.cfg.import.max_refs)?;
    let id = source_id(&request.snapshot.source)?;
    if let Some(id) = &id {
        principal.require_read(id.owner(), id.name())?;
    }
    let hash = request_hash(&owner, &repo, &request);
    handle.revalidate_refs().await?;
    if let Some(result) = replay_or_pristine(&handle, &request.operation_key, &hash)? {
        return Ok(result.response());
    }
    if handle.manifest().object_format != request.snapshot.object_format {
        return Err(ApiError::BadRequest(
            "source and target object formats must match".into(),
        ));
    }
    let progress = handle.subscribe_progress();
    let threads = state.cfg.cache.bulk_threads;
    let meta = mutation_meta(&headers, &principal);
    let work = async move {
        gitcask_wal::on_bulk_runtime(threads, async move {
            Ok(run_import(state, handle, request, id, hash, meta).await)
        })
        .await?
    };
    if sse::wants_sse(&headers) {
        Ok(sse::envelope(vec![progress], async move {
            let result = work.await?;
            Ok(sse::Rendered::json(
                serde_json::to_vec(&result)
                    .map_err(|e| ApiError::Internal(e.to_string()))?
                    .into(),
                "no-store",
                None,
            ))
        }))
    } else {
        Ok(work.await?.response())
    }
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiptQuery {
    operation_key: String,
    request_hash: String,
}
#[utoipa::path(get,path="/{owner}/{repo}/api/import/receipt",tag="writes",summary="Read a committed import receipt using target read permission only",params(("owner"=String,Path),("repo"=String,Path),ReceiptQuery),responses((status=200,body=ImportResult,description="Matching committed receipt"),(status=404,description="No receipt or target not authorized"),(status=409,description="Key or fingerprint mismatch")),security(("jwt_bearer"=[])))]
pub(crate) async fn receipt(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ReceiptQuery>,
) -> Result<Response, ApiError> {
    let principal = state.auth.authenticate(&headers).await?;
    principal.require_read(&owner, &repo)?;
    operation_key(&query.operation_key)?;
    let id = RepoId::new(&owner, &repo)?;
    let handle = state.registry.open(&id).await?;
    handle.revalidate_refs().await?;
    let manifest = handle.manifest();
    let r = manifest
        .import_receipt
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("import receipt".into()))?;
    if r.operation_key != query.operation_key || r.request_hash != query.request_hash {
        return Err(ApiError::Conflict(
            "import receipt key or fingerprint differs".into(),
        ));
    }
    Ok(ImportResult::from_receipt(r, true).response())
}

async fn run_import(
    state: Arc<AppState>,
    handle: Arc<RepoHandle>,
    request: ImportRequest,
    id: Option<RepoId>,
    hash: String,
    mut meta: HashMap<String, String>,
) -> Result<ImportResult, ApiError> {
    let task = loop {
        match handle.begin_task("import", HashMap::new()) {
            Begin::Started(task) => break task,
            Begin::AlreadyRunning(task) => {
                let mut done = task.done_rx();
                while !*done.borrow() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
                handle.revalidate_refs().await?;
                if let Some(result) = replay_or_pristine(&handle, &request.operation_key, &hash)? {
                    return Ok(result);
                }
            }
        }
    };
    let result = async {
        if let Some(result) = replay_or_pristine(&handle, &request.operation_key, &hash)? {
            return Ok(result);
        }
        task.notice("Acquiring the pinned heads/tags and complete reachable history");
        let (pack, peels) = tokio::time::timeout(
            state.cfg.import.timeout,
            acquire_pack(&state, &handle, &request.snapshot, id, task.reporter()),
        )
        .await
        .map_err(|_| {
            ApiError::ImportUnavailable("import deadline exceeded; retry the fixed snapshot".into())
        })??;
        let mut updates: Vec<_> = request
            .snapshot
            .refs
            .iter()
            .zip(peels)
            .map(|(r, peeled)| RefUpdate {
                name: r.name.clone(),
                new_oid: r.oid.clone(),
                new_peeled: peeled,
                ..Default::default()
            })
            .collect();
        updates.push(RefUpdate {
            name: "HEAD".into(),
            new_symbolic_target: request.snapshot.head.symbolic_target.clone(),
            new_oid: if request.snapshot.head.symbolic_target.is_empty() {
                request.snapshot.head.oid.clone()
            } else {
                String::new()
            },
            ..Default::default()
        });
        let mut receipt = ImportReceipt {
            operation_key: request.operation_key,
            request_hash: hash,
            snapshot_hash: request.snapshot.snapshot_hash,
            seq: 0,
            refs_count: request.snapshot.refs.len() as u64,
            head_target: request.snapshot.head.symbolic_target,
            head_oid: request.snapshot.head.oid,
        };
        meta.insert("operation".into(), "import".into());
        meta.insert("import_request_hash".into(), receipt.request_hash.clone());
        task.notice("Publishing full history, heads/tags, HEAD and receipt through manifest CAS");
        let published = handle
            .publish_import(
                pack,
                RefTransaction {
                    updates,
                    atomic: true,
                    ..Default::default()
                },
                receipt.clone(),
                meta,
            )
            .await?;
        if published.per_ref.iter().any(|(_, r)| r.is_err()) {
            handle.revalidate_refs().await?;
            if let Some(result) =
                replay_or_pristine(&handle, &receipt.operation_key, &receipt.request_hash)?
            {
                return Ok(result);
            }
            return Err(ApiError::Conflict(
                "repository changed during import".into(),
            ));
        }
        receipt.seq = published.seq;
        Ok(ImportResult::from_receipt(&receipt, false))
    }
    .await;
    match &result {
        Ok(value) => task.finish_ok("Repository imported", serde_json::to_value(value).ok()),
        Err(error) => task.finish_err(error.status().as_u16(), error.message()),
    };
    result
}

async fn acquire_pack(
    state: &AppState,
    target: &RepoHandle,
    snapshot: &ImportSnapshot,
    id: Option<RepoId>,
    reporter: gitcask_wal::progress::Reporter,
) -> Result<(gitcask_git::IngestedPack, Vec<String>), ApiError> {
    let scratch = tokio::task::spawn_blocking(tempfile::tempdir)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let path = scratch.path().to_path_buf();
    let staging_id = RepoId::new("import", "staging")?;
    let staging_path = staging_id.local_dir(&path);
    let format = &snapshot.object_format;
    let mut command = transport::git(&path);
    command
        .args([
            "init",
            "--bare",
            "--template=",
            &format!("--object-format={format}"),
        ])
        .arg(staging_path);
    transport::output(command, 65536).await?;
    let staging = tokio::task::spawn_blocking(move || LocalRepo::open(&path, &staging_id))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??
        .ok_or_else(|| ApiError::Internal("staging repo missing after initialization".into()))?;
    let mut tips: Vec<String> = snapshot.refs.iter().map(|r| r.oid.clone()).collect();
    tips.push(snapshot.head.oid.clone());
    tips.sort();
    tips.dedup();
    let max_bytes = state
        .cfg
        .import
        .max_bytes
        .as_u64()
        .min(state.cfg.server.max_push_bytes.as_u64());
    let options = IngestOptions {
        fsck: true,
        max_bytes: Some(max_bytes),
        thin: false,
    };
    if let Some(id) = id {
        let source = state.registry.open(&id).await?;
        let guard = source.sync_full().await?;
        staging
            .import_pack_from(source.local().path(), Some(&tips), options.clone())
            .await
            .map_err(import_git_error)?
            .ok_or_else(|| ApiError::UnprocessableEntity("empty source pack".into()))?;
        drop(guard);
    } else {
        let relay = Relay::start(&snapshot.source, max_bytes).await?;
        let mut command = transport::git(staging.path());
        command.args([
            "fetch",
            "--stdin",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            &relay.url,
        ]);
        let refspecs = tips
            .iter()
            .enumerate()
            .map(|(i, tip)| format!("{tip}:refs/import/{i}\n"))
            .collect::<String>();
        transport::output_input(command, 65536, Some(refspecs.as_bytes()))
            .await
            .map_err(|error| relay.error(error))?;
    }
    let mut command = transport::git(staging.path());
    command.args(["rev-parse", "--is-shallow-repository"]);
    if transport::output(command, 64).await? != b"false\n" {
        return Err(ApiError::UnprocessableEntity(
            "shallow sources are unsupported; complete history is required".into(),
        ));
    }
    let oids = tips
        .iter()
        .map(|s| {
            gix_hash::ObjectId::from_hex(s.as_bytes())
                .map_err(|e| ApiError::BadRequest(e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Verify staging independently: old uncommitted destination packs must
    // never supply a missing parent/blob and hide an incomplete import pack.
    staging
        .check_connectivity_async(&oids, false)
        .await
        .map_err(import_git_error)?;
    reporter.notice("Checking every acquired historical object for LFS pointers and object limits");
    scan_history(&staging, state.cfg.import.max_objects).await?;
    // Validate advertised branch/HEAD type and peeled tags from actual objects,
    // so a forged snapshot cannot publish incorrect refs-first advertisements.
    let mut peels = Vec::with_capacity(snapshot.refs.len());
    for r in &snapshot.refs {
        let mut command = transport::git(staging.path());
        command.args(["cat-file", "-t", &r.oid]);
        let kind = transport::output(command, 64).await?;
        if r.name.starts_with("refs/heads/") && kind != b"commit\n" {
            return Err(ApiError::BadRequest("branch must point at a commit".into()));
        }
        if kind == b"tag\n" {
            let mut command = transport::git(staging.path());
            command.args(["rev-parse", "--verify", &format!("{}^{{}}", r.oid)]);
            let peeled = transport::output(command, 128).await?;
            let actual = String::from_utf8_lossy(&peeled).trim().to_string();
            if !r.peeled.is_empty() && actual != r.peeled {
                return Err(ApiError::BadRequest("tag peeled oid mismatch".into()));
            }
            peels.push(actual);
        } else if !r.peeled.is_empty() {
            return Err(ApiError::BadRequest(
                "non-tag cannot have peeled oid".into(),
            ));
        } else {
            peels.push(String::new());
        }
    }
    let mut command = transport::git(staging.path());
    command.args(["cat-file", "-t", &snapshot.head.oid]);
    if transport::output(command, 64).await? != b"commit\n" {
        return Err(ApiError::BadRequest("HEAD must point at a commit".into()));
    }
    reporter.notice("Packing the pinned history independently into the target");
    let pack = target
        .local()
        .import_pack_from(staging.path(), Some(&tips), options)
        .await
        .map_err(import_git_error)?
        .ok_or_else(|| ApiError::UnprocessableEntity("empty source pack".into()))?;
    if pack.object_count > state.cfg.import.max_objects {
        return Err(ApiError::PayloadTooLarge);
    }
    // Closure was indexed with fsck in independent staging and repacked without
    // thinning or alternates; verify destination connectivity before CAS too.
    target
        .local()
        .check_connectivity_async(&oids, false)
        .await?;
    tokio::task::spawn_blocking(move || {
        drop(staging);
        drop(scratch);
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((pack, peels))
}

fn import_git_error(error: gitcask_git::GitError) -> ApiError {
    match &error {
        gitcask_git::GitError::InvalidInput(message)
            if message.starts_with("pack exceeds max_bytes") =>
        {
            ApiError::PayloadTooLarge
        }
        gitcask_git::GitError::MissingObject { .. } | gitcask_git::GitError::Subprocess { .. } => {
            ApiError::Conflict("source_snapshot_unavailable: pinned closure unavailable".into())
        }
        _ => error.into(),
    }
}

async fn scan_history(local: &LocalRepo, max_objects: u64) -> Result<(), ApiError> {
    let mut listing = transport::git(local.path());
    listing.args([
        "cat-file",
        "--batch-all-objects",
        "--batch-check=%(objectname) %(objecttype) %(objectsize)",
    ]);
    let mut listing = listing
        .spawn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut lines = BufReader::new(
        listing
            .stdout
            .take()
            .ok_or_else(|| ApiError::Internal("object listing stdout".into()))?,
    );
    let mut reader = transport::git(local.path());
    reader.args(["cat-file", "--batch"]).stdin(Stdio::piped());
    let mut reader = reader
        .spawn()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut input = reader
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("object reader stdin".into()))?;
    let mut output = BufReader::new(
        reader
            .stdout
            .take()
            .ok_or_else(|| ApiError::Internal("object reader stdout".into()))?,
    );
    let io = |e: std::io::Error| ApiError::Internal(e.to_string());
    let mut count = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).await.map_err(io)? == 0 {
            break;
        }
        count += 1;
        if count > max_objects {
            return Err(ApiError::PayloadTooLarge);
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let [oid, kind, size_text] = fields.as_slice() else {
            return Err(ApiError::Internal("invalid object listing".into()));
        };
        let size = size_text
            .parse::<usize>()
            .map_err(|_| ApiError::Internal("invalid object size".into()))?;
        if *kind != "blob" || size > 1024 {
            continue;
        }
        input
            .write_all(format!("{oid}\n").as_bytes())
            .await
            .map_err(io)?;
        let mut header = String::new();
        output.read_line(&mut header).await.map_err(io)?;
        if header.trim() != format!("{oid} blob {size}") {
            return Err(ApiError::Internal("invalid object reader header".into()));
        }
        let mut blob = vec![0; size + 1];
        output.read_exact(&mut blob).await.map_err(io)?;
        if super::initialize::has_lfs_pointer_header(&blob) {
            return Err(ApiError::UnprocessableEntity(
                "Git LFS pointer in source history; full-history import does not copy LFS payloads"
                    .into(),
            ));
        }
    }
    drop(input);
    if !listing.wait().await.map_err(io)?.success() || !reader.wait().await.map_err(io)?.success() {
        return Err(ApiError::Conflict("source object audit failed".into()));
    }
    Ok(())
}

#[cfg(test)]
mod hash_tests {
    use super::*;
    #[test]
    fn canonical_hashes_bind_target_without_json_serialization_rules() -> anyhow::Result<()> {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/import-hashes.json"))?;
        let snapshot: ImportSnapshot = serde_json::from_value(fixture["snapshot"].clone())?;
        validate_snapshot(&snapshot, 1024).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let request = ImportRequest {
            operation_key: "creation-123".into(),
            snapshot,
        };
        assert_eq!(
            request_hash("project", "repository", &request),
            fixture["request_hash"].as_str().unwrap_or_default()
        );
        assert_ne!(
            request_hash("another", "repository", &request),
            fixture["request_hash"].as_str().unwrap_or_default()
        );
        Ok(())
    }
}
