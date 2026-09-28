//! Copy a pinned tree as an independently stored root commit.

use std::{collections::HashMap, process::Stdio, sync::Arc};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use gitcask_proto::v1::Initialization;
use gitcask_wal::{Begin, RepoHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use utoipa::ToSchema;

use crate::{AppState, error::ApiError, sse};

use super::{
    commit::{
        CommitIdentity, commit_tree, ensure_git_success, pack_commit_objects_into, run_git,
        validate_identity,
    },
    write::{mutation_meta, open_write, qualify_ref},
};

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitializeSource {
    owner: String,
    repo: String,
    /// Full pinned commit ID; branch names, tags and abbreviations are refused.
    commit_oid: String,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitializeRequest {
    source: InitializeSource,
    /// Branch below `refs/heads/`; also becomes the target of HEAD.
    branch: String,
    /// 1–128 printable ASCII characters, without whitespace. Reuse on retry.
    operation_key: String,
    message: String,
    /// Defaults to committer. All commit times are explicitly supplied.
    author: Option<CommitIdentity>,
    committer: CommitIdentity,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InitializeResult {
    #[serde(rename = "ref")]
    ref_name: String,
    commit_oid: String,
    tree_oid: String,
    seq: u64,
    replayed: bool,
}

impl InitializeResult {
    fn from_receipt(receipt: &Initialization, replayed: bool) -> Self {
        Self {
            ref_name: receipt.ref_name.clone(),
            commit_oid: receipt.commit_oid.clone(),
            tree_oid: receipt.tree_oid.clone(),
            seq: receipt.seq,
            replayed,
        }
    }

    fn response(self) -> Response {
        let status = if self.replayed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        };
        (status, [("cache-control", "no-store")], Json(self)).into_response()
    }
}

#[utoipa::path(
    post,
    path = "/{owner}/{repo}/api/initialize",
    tag = "writes",
    summary = "Initialize a pristine repository from a pinned Gitcask tree",
    description = "Creates one root commit with the source tree and no source history. Requires destination write and source read permission, matching object formats, and an existing pristine destination. The operation key and canonical request are atomically recorded in the manifest; exact retries return the original result without changing later writes or opening the source. Git LFS pointers are rejected; gitlinks are preserved without recursive submodule fetching. Accept: text/event-stream enables task progress and a terminal result/error envelope.",
    params(
        ("owner" = String, Path, description = "Destination owner"),
        ("repo" = String, Path, description = "Destination repository")
    ),
    request_body = InitializeRequest,
    responses(
        (status = 201, description = "Root commit published", body = InitializeResult),
        (status = 200, description = "Exact replay, or SSE progress envelope", body = InitializeResult),
        (status = 400, description = "Invalid input, non-commit source, or object format mismatch"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Forwarded identity cannot write"),
        (status = 404, description = "Missing or unauthorized repository, or missing commit"),
        (status = 409, description = "Not pristine, or initialization key/payload differs"),
        (status = 413, description = "Body or pack limit exceeded"),
        (status = 422, description = "Git LFS pointer in source tree"),
        (status = 503, description = "Temporary service failure; retry the same request")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn initialize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    request: Result<Json<InitializeRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(mut request) = request.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::PayloadTooLarge
        } else {
            ApiError::BadRequest(error.body_text())
        }
    })?;
    let (destination, principal) = open_write(&state, &headers, &owner, &repo).await?;
    principal.require_read(&request.source.owner, &request.source.repo)?;
    let source_id = gitcask_git::RepoId::new(&request.source.owner, &request.source.repo)
        .map_err(|_| ApiError::BadRequest("invalid source repository".into()))?;
    let ref_name = qualify_ref("heads", &request.branch)?;
    validate_request(&mut request)?;
    let request_hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&request).map_err(|e| ApiError::Internal(e.to_string()))?,
    ));
    drop(destination.sync_refs_only().await?);
    if let Some(result) = replay_or_pristine(&destination, &request.operation_key, &request_hash)? {
        return Ok(result.response());
    }

    let progress = destination.subscribe_progress();
    let meta = mutation_meta(&headers, &principal);
    let threads = state.cfg.cache.bulk_threads;
    let work = async move {
        gitcask_wal::on_bulk_runtime(threads, async move {
            Ok(run_initialization(
                state,
                destination,
                source_id,
                request,
                ref_name,
                request_hash,
                meta,
            )
            .await)
        })
        .await?
    };
    if sse::wants_sse(&headers) {
        Ok(sse::envelope(vec![progress], async move {
            let result = work.await?;
            let body =
                serde_json::to_vec(&result).map_err(|e| ApiError::Internal(e.to_string()))?;
            Ok(sse::Rendered::json(body.into(), "no-store", None))
        }))
    } else {
        Ok(work.await?.response())
    }
}

fn validate_request(request: &mut InitializeRequest) -> Result<(), ApiError> {
    if request.operation_key.is_empty()
        || request.operation_key.len() > 128
        || !request.operation_key.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(ApiError::BadRequest("invalid operation_key".into()));
    }
    gitcask_git::validate_oid(&request.source.commit_oid)?;
    if request.source.commit_oid.bytes().all(|b| b == b'0') {
        return Err(ApiError::BadRequest(
            "source.commit_oid must be a nonzero full commit ID".into(),
        ));
    }
    request.source.commit_oid.make_ascii_lowercase();
    validate_identity(&request.committer, "committer")?;
    if let Some(author) = &request.author {
        validate_identity(author, "author")?;
    } else {
        request.author = Some(request.committer.clone());
    }
    Ok(())
}

fn replay_or_pristine(
    destination: &RepoHandle,
    operation_key: &str,
    request_hash: &str,
) -> Result<Option<InitializeResult>, ApiError> {
    let manifest = destination.manifest();
    if let Some(receipt) = &manifest.initialization {
        if receipt.operation_key == operation_key && receipt.request_hash == request_hash {
            return Ok(Some(InitializeResult::from_receipt(receipt, true)));
        }
        return Err(ApiError::Conflict(
            "initialization operation key or payload differs".into(),
        ));
    }
    if !gitcask_wal::is_pristine(&manifest) {
        return Err(ApiError::Conflict("repository is not pristine".into()));
    }
    Ok(None)
}

async fn run_initialization(
    state: Arc<AppState>,
    destination: Arc<RepoHandle>,
    source_id: gitcask_git::RepoId,
    request: InitializeRequest,
    ref_name: String,
    request_hash: String,
    mut meta: HashMap<String, String>,
) -> Result<InitializeResult, ApiError> {
    // The shared task bounds work per destination on this instance. The WAL CAS
    // remains the cross-instance guard; a follower never reuses another payload.
    let task = loop {
        match destination.begin_task("initialize", HashMap::new()) {
            Begin::Started(task) => break task,
            Begin::AlreadyRunning(task) => {
                let mut done = task.done_rx();
                while !*done.borrow() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
                drop(destination.sync_refs_only().await?);
                if let Some(result) =
                    replay_or_pristine(&destination, &request.operation_key, &request_hash)?
                {
                    return Ok(result);
                }
            }
        }
    };
    let result = async {
        // A task may have completed between the handler precheck and begin_task.
        if let Some(result) =
            replay_or_pristine(&destination, &request.operation_key, &request_hash)?
        {
            return Ok(result);
        }
        task.notice("Materializing the pinned source commit");
        let source = state.registry.open(&source_id).await?;
        if source.local().object_format() != destination.local().object_format()
            || request.source.commit_oid.len()
                != match source.local().object_format() {
                    gitcask_git::ObjectFormat::Sha1 => 40,
                    gitcask_git::ObjectFormat::Sha256 => 64,
                }
        {
            return Err(ApiError::BadRequest(
                "source and destination object formats must match the pinned commit ID".into(),
            ));
        }
        let source_guard = source.sync_full().await?;
        let source_local = source.local().clone();
        let pinned = request.source.commit_oid.clone();
        let tree = tokio::task::spawn_blocking(move || {
            let kind = run_git(
                source_local.path(),
                &["--no-replace-objects", "cat-file", "-t", &pinned],
                &[],
                None,
            )?;
            if !kind.status.success() {
                return Err(ApiError::NotFound("source commit".into()));
            }
            if kind.stdout != b"commit\n" {
                return Err(ApiError::BadRequest(
                    "source object must be a commit".into(),
                ));
            }
            // A full OID pins the actual object, regardless of source refs/replace.
            let expression = format!("{pinned}^{{tree}}");
            let output = run_git(
                source_local.path(),
                &[
                    "--no-replace-objects",
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &expression,
                ],
                &[],
                None,
            )?;
            ensure_git_success(output.status, &output.stderr, "git rev-parse pinned tree")?;
            let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
            gitcask_git::validate_oid(&tree)?;
            Ok::<_, ApiError>(tree)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
        task.notice("Checking the source tree for unsupported LFS pointers");
        reject_lfs_pointers(source.local(), &tree).await?;
        let local = source.local().clone();
        let tree_for_commit = tree.clone();
        let (scratch, staging, commit) = tokio::task::spawn_blocking(move || {
            let scratch = tempfile::tempdir().map_err(|e| ApiError::Internal(e.to_string()))?;
            let staging = gitcask_git::LocalRepo::init(
                scratch.path(),
                &gitcask_git::RepoId::new("initialize", "staging")?,
                local.object_format(),
            )?;
            // A temporary, bare object-writing context. The source stays read-only;
            // neither this alternate nor its path ever reaches the destination.
            std::fs::write(
                staging.path().join("objects/info/alternates"),
                format!("{}\n", local.path().join("objects").display()),
            )
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            let commit = commit_tree(
                staging.path(),
                &tree_for_commit,
                &[],
                &request.message,
                request.author.as_ref().unwrap_or(&request.committer),
                &request.committer,
            )?;
            Ok::<_, ApiError>((scratch, staging, commit))
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
        task.notice("Packing the root commit and its complete tree into the destination");
        let pack =
            pack_commit_objects_into(&state, &staging, destination.local(), &commit, &[]).await?;
        // Cleanup stays off the async workers too.
        tokio::task::spawn_blocking(move || {
            drop(staging);
            drop(scratch);
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
        drop(source_guard);
        destination
            .local()
            .check_connectivity_async(
                &[gix_hash::ObjectId::from_hex(commit.as_bytes())
                    .map_err(|e| ApiError::Internal(e.to_string()))?],
                false,
            )
            .await?;
        let mut receipt = Initialization {
            operation_key: request.operation_key,
            request_hash,
            ref_name,
            commit_oid: commit,
            tree_oid: tree,
            seq: 0,
        };
        meta.insert("operation".into(), "initialize".into());
        meta.insert(
            "initialization_request_hash".into(),
            receipt.request_hash.clone(),
        );
        task.notice("Publishing the destination through the WAL manifest CAS");
        let published = destination
            .publish_initialization(pack, receipt.clone(), meta)
            .await?;
        if published
            .per_ref
            .iter()
            .any(|(_, outcome)| outcome.is_err())
        {
            // Another initializer may have won, including an identical request.
            // Only this failure path needs an additional freshness round trip.
            drop(destination.sync_refs_only().await?);
            if let Some(result) =
                replay_or_pristine(&destination, &receipt.operation_key, &receipt.request_hash)?
            {
                return Ok(result);
            }
            return Err(ApiError::Conflict(
                "repository changed during initialization".into(),
            ));
        }
        receipt.seq = published.seq;
        Ok(InitializeResult::from_receipt(&receipt, false))
    }
    .await;
    match &result {
        Ok(value) => {
            task.finish_ok("Repository initialized", serde_json::to_value(value).ok());
        }
        Err(error) => {
            task.finish_err(error.status().as_u16(), error.message());
        }
    }
    result
}

/// Traverse only the pinned tree. Git LFS pointers are <1024 bytes; batch-read
/// at most 1024 bytes per blob, with constant memory and no checkout/URL fetch.
async fn reject_lfs_pointers(local: &gitcask_git::LocalRepo, tree: &str) -> Result<(), ApiError> {
    let command = |args: &[&str]| {
        let mut command = tokio::process::Command::new("git");
        command
            .current_dir(local.path())
            .env("GIT_DIR", local.path())
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
    };
    let io_error = |e: std::io::Error| ApiError::Internal(format!("LFS pointer scan: {e}"));
    let mut listing = command(&["ls-tree", "-r", "-l", "-z", tree])
        .spawn()
        .map_err(io_error)?;
    let mut files = command(&["cat-file", "--batch"])
        .spawn()
        .map_err(io_error)?;
    let mut entries = BufReader::new(
        listing
            .stdout
            .take()
            .ok_or_else(|| ApiError::Internal("ls-tree stdout".into()))?,
    );
    let mut input = files
        .stdin
        .take()
        .ok_or_else(|| ApiError::Internal("cat-file stdin".into()))?;
    let mut output = BufReader::new(
        files
            .stdout
            .take()
            .ok_or_else(|| ApiError::Internal("cat-file stdout".into()))?,
    );
    let mut record = Vec::new();
    loop {
        record.clear();
        if entries.read_until(0, &mut record).await.map_err(io_error)? == 0 {
            break;
        }
        let header = record.split(|b| *b == b'\t').next().unwrap_or_default();
        let mut fields = header
            .split(u8::is_ascii_whitespace)
            .filter(|f| !f.is_empty());
        let mode = fields.next();
        let kind = fields.next();
        let oid = fields.next().unwrap_or_default();
        let size = fields
            .next()
            .and_then(|f| std::str::from_utf8(f).ok())
            .and_then(|f| f.parse::<usize>().ok());
        if kind != Some(b"blob".as_slice()) || mode == Some(b"120000".as_slice()) {
            continue;
        }
        let size = size.ok_or_else(|| ApiError::Internal("invalid ls-tree blob size".into()))?;
        if size > 1024 {
            continue;
        }
        input.write_all(oid).await.map_err(io_error)?;
        input.write_all(b"\n").await.map_err(io_error)?;
        let mut header = String::new();
        output.read_line(&mut header).await.map_err(io_error)?;
        if header.trim_end() != format!("{} blob {size}", String::from_utf8_lossy(oid)) {
            return Err(ApiError::Internal("invalid cat-file blob header".into()));
        }
        let mut blob = vec![0; size + 1];
        output.read_exact(&mut blob).await.map_err(io_error)?;
        if has_lfs_pointer_header(&blob) {
            return Err(ApiError::UnprocessableEntity(
                "source tree contains a Git LFS pointer; initialization does not copy LFS payloads"
                    .into(),
            ));
        }
    }
    drop(input);
    if !listing.wait().await.map_err(io_error)?.success()
        || !files.wait().await.map_err(io_error)?.success()
    {
        return Err(ApiError::Internal(
            "git failed while checking LFS pointers".into(),
        ));
    }
    Ok(())
}

fn has_lfs_pointer_header(blob: &[u8]) -> bool {
    // git-lfs DecodeFrom trims surrounding Unicode whitespace, accepts CRLF,
    // and recognizes all three version URLs. Its decoder also allows extension
    // lines before the version. Reject these headers conservatively, even when
    // the remaining pointer fields are malformed; no LFS payload is copied.
    let text = String::from_utf8_lossy(blob);
    let header = text
        .trim()
        .lines()
        .find(|line| !line.is_empty() && !line.starts_with("ext-"));
    matches!(
        header,
        Some(
            "version http://git-media.io/v/2"
                | "version https://hawser.github.com/spec/v1"
                | "version https://git-lfs.github.com/spec/v1"
        )
    )
}
