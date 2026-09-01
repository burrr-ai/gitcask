//! Branch and tag mutations through the existing WAL publisher.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use utoipa::{IntoParams, ToSchema};

use crate::{AppState, auth::Principal, error::ApiError};

use super::git::{GitFailure, git};

#[derive(Deserialize, ToSchema)]
pub(crate) struct RefWriteRequest {
    /// Commit oid or revision name to which the ref will point.
    target: String,
    /// Optimistic-concurrency guard. All-zero means the ref must not exist.
    /// When omitted, the update is forced to the latest observed value.
    expected_old_oid: Option<String>,
}

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RefDeleteQuery {
    /// Optimistic-concurrency guard. When omitted, deletion is forced.
    expected_old_oid: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AnnotatedTagRequest {
    name: String,
    /// Commit oid or revision name to tag.
    target: String,
    message: String,
    tagger: Tagger,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct Tagger {
    name: String,
    email: String,
    /// RFC 3339 timestamp, including an explicit offset or `Z`.
    when: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RefMutation {
    #[serde(rename = "ref")]
    ref_name: String,
    /// Oid stored directly in the ref (the tag-object oid for annotated tags).
    oid: String,
    /// Peeled commit oid for an annotated tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    peeled: Option<String>,
    /// WAL sequence; zero means the requested ref already had this value.
    seq: u64,
}

pub(super) struct PreparedMutation {
    pub(super) ref_name: String,
    pub(super) new_oid: String,
    pub(super) new_peeled: String,
    pub(super) expected_old_oid: Option<String>,
    pub(super) pack: Option<gitcask_git::IngestedPack>,
    pub(super) deleting: bool,
}

#[utoipa::path(
    put,
    path = "/{owner}/{repo}/api/refs/heads/{name}",
    tag = "writes",
    summary = "Create or move a branch",
    description = "`expected_old_oid` enables compare-and-swap; an all-zero oid requires creation. Omitting it performs a force update against the latest observed ref value.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("name" = String, Path, description = "Branch name, including optional slash-separated components")
    ),
    request_body = RefWriteRequest,
    responses(
        (status = 200, description = "Branch created or moved", body = RefMutation),
        (status = 400, description = "Invalid ref, oid, or non-commit target"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or target not found"),
        (status = 409, description = "Expected old oid did not match"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn put_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Json(request): Json<RefWriteRequest>,
) -> Result<Response, ApiError> {
    put_ref(state, headers, owner, repo, name, request, "heads").await
}

#[utoipa::path(
    put,
    path = "/{owner}/{repo}/api/refs/tags/{name}",
    tag = "writes",
    summary = "Create or move a lightweight tag",
    description = "Creates a lightweight tag pointing directly at a commit. `expected_old_oid` enables compare-and-swap; omitting it performs a force update.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("name" = String, Path, description = "Tag name, including optional slash-separated components")
    ),
    request_body = RefWriteRequest,
    responses(
        (status = 200, description = "Lightweight tag created or moved", body = RefMutation),
        (status = 400, description = "Invalid ref, oid, or non-commit target"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or target not found"),
        (status = 409, description = "Expected old oid did not match"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn put_lightweight_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Json(request): Json<RefWriteRequest>,
) -> Result<Response, ApiError> {
    put_ref(state, headers, owner, repo, name, request, "tags").await
}

async fn put_ref(
    state: Arc<AppState>,
    headers: HeaderMap,
    owner: String,
    repo: String,
    name: String,
    request: RefWriteRequest,
    namespace: &str,
) -> Result<Response, ApiError> {
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let ref_name = qualify_ref(namespace, &name)?;
    validate_expected(request.expected_old_oid.as_deref())?;
    let expected_old_oid = request.expected_old_oid.map(|oid| oid.to_ascii_lowercase());
    let guard = handle.sync_full().await?;
    let target = resolve_commit_target(handle.local(), &request.target).await?;
    drop(guard);
    let result = publish_mutation(
        &handle,
        PreparedMutation {
            ref_name: ref_name.clone(),
            new_oid: target.clone(),
            new_peeled: String::new(),
            expected_old_oid,
            pack: None,
            deleting: false,
        },
        mutation_meta(&headers, &principal),
    )
    .await?;
    Ok(Json(RefMutation {
        ref_name,
        oid: target,
        peeled: None,
        seq: result,
    })
    .into_response())
}

#[utoipa::path(
    delete,
    path = "/{owner}/{repo}/api/refs/heads/{name}",
    tag = "writes",
    summary = "Delete a branch",
    description = "`expected_old_oid` is an optional query-string compare-and-swap guard. Omitting it force-deletes the latest observed value.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("name" = String, Path, description = "Branch name, including optional slash-separated components"),
        RefDeleteQuery
    ),
    responses(
        (status = 204, description = "Branch deleted"),
        (status = 400, description = "Invalid ref or oid"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or branch not found"),
        (status = 409, description = "Expected old oid did not match"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn delete_branch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Query(query): Query<RefDeleteQuery>,
) -> Result<StatusCode, ApiError> {
    delete_ref(state, headers, owner, repo, name, query, "heads").await
}

#[utoipa::path(
    delete,
    path = "/{owner}/{repo}/api/refs/tags/{name}",
    tag = "writes",
    summary = "Delete a tag",
    description = "`expected_old_oid` is an optional query-string compare-and-swap guard. Omitting it force-deletes the latest observed value.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("name" = String, Path, description = "Tag name, including optional slash-separated components"),
        RefDeleteQuery
    ),
    responses(
        (status = 204, description = "Tag deleted"),
        (status = 400, description = "Invalid ref or oid"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or tag not found"),
        (status = 409, description = "Expected old oid did not match"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn delete_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Query(query): Query<RefDeleteQuery>,
) -> Result<StatusCode, ApiError> {
    delete_ref(state, headers, owner, repo, name, query, "tags").await
}

async fn delete_ref(
    state: Arc<AppState>,
    headers: HeaderMap,
    owner: String,
    repo: String,
    name: String,
    query: RefDeleteQuery,
    namespace: &str,
) -> Result<StatusCode, ApiError> {
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let ref_name = qualify_ref(namespace, &name)?;
    validate_expected(query.expected_old_oid.as_deref())?;
    let expected_old_oid = query.expected_old_oid.map(|oid| oid.to_ascii_lowercase());
    drop(handle.sync_refs().await?);
    publish_mutation(
        &handle,
        PreparedMutation {
            ref_name,
            new_oid: String::new(),
            new_peeled: String::new(),
            expected_old_oid,
            pack: None,
            deleting: true,
        },
        mutation_meta(&headers, &principal),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/{owner}/{repo}/api/tags",
    tag = "writes",
    summary = "Create an annotated tag",
    description = "Creates one Git tag object, publishes it as a one-object pack, then atomically creates the tag ref. The tag must not already exist.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    request_body = AnnotatedTagRequest,
    responses(
        (status = 201, description = "Annotated tag created", body = RefMutation),
        (status = 400, description = "Invalid tag, tagger, timestamp, or non-commit target"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or target not found"),
        (status = 409, description = "Tag already exists"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn create_annotated_tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(request): Json<AnnotatedTagRequest>,
) -> Result<Response, ApiError> {
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let ref_name = qualify_ref("tags", &request.name)?;
    validate_tagger(&request.tagger)?;
    let guard = handle.sync_full().await?;
    let target = resolve_commit_target(handle.local(), &request.target).await?;
    let tag_oid = create_tag_object(handle.local(), &request, &target).await?;
    let pack = pack_tag_object(&state, handle.local(), &tag_oid).await?;
    drop(guard);
    let seq = publish_mutation(
        &handle,
        PreparedMutation {
            ref_name: ref_name.clone(),
            new_oid: tag_oid.clone(),
            new_peeled: target.clone(),
            expected_old_oid: Some(String::new()),
            pack: Some(pack),
            deleting: false,
        },
        mutation_meta(&headers, &principal),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(RefMutation {
            ref_name,
            oid: tag_oid,
            peeled: Some(target),
            seq,
        }),
    )
        .into_response())
}

pub(super) async fn open_write(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    repo: &str,
) -> Result<(Arc<gitcask_wal::RepoHandle>, Principal), ApiError> {
    let principal = state.auth.require_write(headers, owner, repo).await?;
    let id = gitcask_git::RepoId::new(owner, repo)
        .map_err(|_| ApiError::NotFound("repository".into()))?;
    let handle = state.registry.open(&id).await?;
    Ok((handle, principal))
}

pub(super) fn qualify_ref(namespace: &str, name: &str) -> Result<String, ApiError> {
    let ref_name = format!("refs/{namespace}/{name}");
    gitcask_git::validate_ref_name(&ref_name)?;
    Ok(ref_name)
}

pub(super) fn validate_expected(expected: Option<&str>) -> Result<(), ApiError> {
    if let Some(expected) = expected {
        gitcask_git::validate_oid(expected)?;
    }
    Ok(())
}

pub(super) async fn current_oid(
    handle: &Arc<gitcask_wal::RepoHandle>,
    ref_name: &str,
) -> Result<Option<String>, ApiError> {
    let local = handle.local().clone();
    let ref_name = ref_name.to_string();
    let current =
        tokio::task::spawn_blocking(move || local.ref_view().map(|refs| refs.get(&ref_name)))
            .await
            .map_err(|error| ApiError::Internal(format!("ref lookup task: {error}")))??;
    Ok(current)
}

pub(super) async fn publish_mutation(
    handle: &Arc<gitcask_wal::RepoHandle>,
    mutation: PreparedMutation,
    meta: HashMap<String, String>,
) -> Result<u64, ApiError> {
    let attempts = handle.config().wal.cas_max_retries.max(1);
    for attempt in 0..attempts {
        let current = current_oid(handle, &mutation.ref_name).await?;
        if mutation.deleting && current.is_none() {
            return match mutation.expected_old_oid.as_deref() {
                Some(expected) if !is_null_oid(expected) => Err(ApiError::Conflict(format!(
                    "expected {expected}, got missing ref"
                ))),
                _ => Err(ApiError::NotFound(mutation.ref_name.clone())),
            };
        }
        let old_oid = mutation
            .expected_old_oid
            .clone()
            .unwrap_or_else(|| current.clone().unwrap_or_default());
        if !mutation.deleting && current.as_deref() == Some(mutation.new_oid.as_str()) {
            if let Some(expected) = mutation.expected_old_oid.as_deref()
                && !oid_matches(expected, current.as_deref())
            {
                return Err(ApiError::Conflict(format!(
                    "expected {expected}, got {}",
                    current.as_deref().unwrap_or("missing ref")
                )));
            }
            return Ok(0);
        }
        let update = gitcask_proto::v1::RefUpdate {
            name: mutation.ref_name.clone(),
            old_oid,
            new_oid: mutation.new_oid.clone(),
            new_symbolic_target: String::new(),
            new_peeled: mutation.new_peeled.clone(),
        };
        gitcask_git::validate_ref_update(&update)?;
        let txn = gitcask_proto::v1::RefTransaction {
            updates: vec![update],
            push_options: Vec::new(),
            atomic: true,
        };
        let result = handle
            .publish_push_synced(mutation.pack.clone(), txn, meta.clone())
            .await?;
        let Some((_, per_ref)) = result.per_ref.into_iter().next() else {
            return Err(ApiError::Internal(
                "publisher returned no ref result".into(),
            ));
        };
        match per_ref {
            Ok(()) => return Ok(result.seq),
            Err(gitcask_wal::RefError::Conflict { expected, actual })
                if mutation.expected_old_oid.is_none() =>
            {
                tracing::debug!(ref_name = %mutation.ref_name, expected, actual, "force ref update raced; retrying");
                if attempt + 1 < attempts {
                    tokio::time::sleep(gitcask_store::util::backoff(
                        attempt,
                        std::time::Duration::from_millis(5),
                        std::time::Duration::from_millis(250),
                    ))
                    .await;
                }
            }
            Err(gitcask_wal::RefError::Conflict { expected, actual }) => {
                return Err(ApiError::Conflict(format!(
                    "expected {expected}, got {actual}"
                )));
            }
            Err(error) => return Err(ApiError::BadRequest(error.to_string())),
        }
    }
    Err(ApiError::Conflict(
        "ref kept changing while applying force update".into(),
    ))
}

pub(super) fn mutation_meta(headers: &HeaderMap, principal: &Principal) -> HashMap<String, String> {
    let mut meta = HashMap::from([
        ("agent".to_string(), "gitcask-api".to_string()),
        ("principal".to_string(), principal.name.clone()),
        ("push_options".to_string(), String::new()),
    ]);
    if let Some(request_id) = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
    {
        meta.insert("request_id".to_string(), request_id.to_string());
    }
    meta
}

fn is_null_oid(oid: &str) -> bool {
    oid.is_empty() || oid.bytes().all(|byte| byte == b'0')
}

fn oid_matches(expected: &str, actual: Option<&str>) -> bool {
    if is_null_oid(expected) {
        actual.is_none()
    } else {
        actual == Some(expected)
    }
}

pub(super) async fn resolve_commit_target(
    local: &gitcask_git::LocalRepo,
    target: &str,
) -> Result<String, ApiError> {
    if target.is_empty() || target.starts_with('-') {
        return Err(ApiError::NotFound(format!("unknown target {target}")));
    }
    let object = format!("{target}^{{object}}");
    git(
        local,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            object,
        ],
        GitFailure::NotFound,
    )
    .await
    .map_err(|error| match error {
        ApiError::NotFound(_) => ApiError::NotFound(format!("unknown target {target}")),
        other => other,
    })?;
    let commit = format!("{target}^{{commit}}");
    let output = git(
        local,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            commit,
        ],
        GitFailure::NotFound,
    )
    .await
    .map_err(|error| match error {
        ApiError::NotFound(_) => ApiError::BadRequest(format!("target {target} is not a commit")),
        other => other,
    })?;
    let oid = String::from_utf8_lossy(&output).trim().to_string();
    gitcask_git::validate_oid(&oid)?;
    Ok(oid)
}

fn validate_tagger(tagger: &Tagger) -> Result<(), ApiError> {
    let invalid_name = tagger.name.is_empty()
        || tagger
            .name
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'<' | b'>'));
    let invalid_email = tagger.email.is_empty()
        || tagger
            .email
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'<' | b'>'));
    if invalid_name || invalid_email {
        return Err(ApiError::BadRequest("invalid tagger identity".into()));
    }
    chrono::DateTime::parse_from_rfc3339(&tagger.when)
        .map_err(|_| ApiError::BadRequest("tagger.when must be RFC 3339".into()))?;
    Ok(())
}

fn git_tagger_time(when: &str) -> Result<String, ApiError> {
    let parsed = chrono::DateTime::parse_from_rfc3339(when)
        .map_err(|_| ApiError::BadRequest("tagger.when must be RFC 3339".into()))?;
    let offset = parsed.offset().local_minus_utc();
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    Ok(format!(
        "{} {sign}{:02}{:02}",
        parsed.timestamp(),
        minutes / 60,
        minutes % 60
    ))
}

async fn create_tag_object(
    local: &gitcask_git::LocalRepo,
    request: &AnnotatedTagRequest,
    target: &str,
) -> Result<String, ApiError> {
    let when = git_tagger_time(&request.tagger.when)?;
    let mut object = format!(
        "object {target}\ntype commit\ntag {}\ntagger {} <{}> {when}\n\n{}",
        request.name, request.tagger.name, request.tagger.email, request.message
    );
    if !object.ends_with('\n') {
        object.push('\n');
    }
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .arg("mktag")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| ApiError::Internal(format!("git mktag: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("git mktag stdin unavailable".into()))?;
    stdin
        .write_all(object.as_bytes())
        .await
        .map_err(|error| ApiError::Internal(format!("git mktag stdin: {error}")))?;
    stdin
        .shutdown()
        .await
        .map_err(|error| ApiError::Internal(format!("git mktag stdin: {error}")))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| ApiError::Internal(format!("git mktag: {error}")))?;
    if !output.status.success() {
        return Err(ApiError::BadRequest(format!(
            "invalid annotated tag: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    gitcask_git::validate_oid(&oid)?;
    Ok(oid)
}

async fn pack_tag_object(
    state: &AppState,
    local: &gitcask_git::LocalRepo,
    oid: &str,
) -> Result<gitcask_git::IngestedPack, ApiError> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .args(["pack-objects", "--stdout"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| ApiError::Internal(format!("git pack-objects: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("git pack-objects stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("git pack-objects stdout unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::Internal("git pack-objects stderr unavailable".into()))?;
    let input = format!("{oid}\n");
    let feed = async move {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await
    };
    let read_stderr = async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    };
    let ingest = local.ingest_pack(
        stdout,
        gitcask_git::IngestOptions {
            fsck: state.cfg.wal.fsck_objects,
            max_bytes: Some(state.cfg.server.max_push_bytes.as_u64()),
            thin: false,
        },
    );
    let (feed_result, ingest_result, stderr_result) = tokio::join!(feed, ingest, read_stderr);
    let status = child
        .wait()
        .await
        .map_err(|error| ApiError::Internal(format!("git pack-objects: {error}")))?;
    feed_result.map_err(|error| ApiError::Internal(format!("git pack-objects stdin: {error}")))?;
    let stderr = stderr_result
        .map_err(|error| ApiError::Internal(format!("git pack-objects stderr: {error}")))?;
    if !status.success() {
        return Err(ApiError::Internal(format!(
            "git pack-objects exited {:?}: {}",
            status.code(),
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    ingest_result?
        .ok_or_else(|| ApiError::Internal("git pack-objects produced an empty pack".into()))
}
