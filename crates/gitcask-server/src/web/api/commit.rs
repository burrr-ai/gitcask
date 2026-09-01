//! Batch commit and merge mutations without a worktree.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::Path as FsPath;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use utoipa::ToSchema;

use crate::{AppState, error::ApiError};

use super::write::{
    PreparedMutation, current_oid, mutation_meta, open_write, publish_mutation, qualify_ref,
    validate_expected,
};

const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone, Deserialize, ToSchema)]
pub(crate) struct CommitIdentity {
    name: String,
    email: String,
    /// RFC 3339 timestamp, including an explicit offset or `Z`.
    when: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum CommitChange {
    Upsert {
        path: String,
        /// Standard base64-encoded blob contents.
        content: String,
        /// Git file mode: `100644`, `100755`, or `120000`.
        mode: String,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CommitRequest {
    /// Branch name below `refs/heads/`.
    branch: String,
    message: String,
    /// Compare-and-swap guard. When omitted, the new commit force-updates the
    /// branch even if another writer moved it after object construction.
    expected_head_oid: Option<String>,
    /// Defaults to `committer` when omitted.
    author: Option<CommitIdentity>,
    committer: CommitIdentity,
    changes: Vec<CommitChange>,
    #[serde(default)]
    allow_empty: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CommitMutation {
    #[serde(rename = "ref")]
    ref_name: String,
    oid: String,
    commit_oid: String,
    seq: u64,
}

#[derive(Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MergeStrategy {
    Merge,
    Squash,
    FastForwardOnly,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct MergeRequest {
    /// Destination branch name below `refs/heads/`.
    base: String,
    /// Commit oid or revision name to merge.
    head: String,
    message: String,
    committer: CommitIdentity,
    strategy: MergeStrategy,
    /// Required compare-and-swap guard for the destination branch.
    expected_base_oid: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MergeMutation {
    #[serde(rename = "ref")]
    ref_name: String,
    oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_oid: Option<String>,
    seq: u64,
    already_merged: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MergeConflictResponse {
    error: &'static str,
    conflicts: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/{owner}/{repo}/api/commits",
    tag = "writes",
    summary = "Commit a batch of file changes",
    description = "Creates blobs, only the changed paths' ancestor trees, and one commit without a worktree, packs the new objects once, then publishes the branch update through the WAL.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    request_body = CommitRequest,
    responses(
        (status = 201, description = "Commit created and branch updated", body = CommitMutation),
        (status = 400, description = "Invalid or empty change set"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository or branch not found"),
        (status = 409, description = "Expected head oid did not match"),
        (status = 413, description = "Change count or decoded content limit exceeded"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn create_commit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(request): Json<CommitRequest>,
) -> Result<Response, ApiError> {
    validate_commit_request(&state, &request)?;
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let ref_name = qualify_ref("heads", &request.branch)?;
    validate_expected(request.expected_head_oid.as_deref())?;
    let expected = request
        .expected_head_oid
        .as_deref()
        .map(str::to_ascii_lowercase);

    let guard = handle.sync_full().await?;
    let parent = current_oid(&handle, &ref_name)
        .await?
        .ok_or_else(|| ApiError::NotFound(ref_name.clone()))?;
    ensure_expected(expected.as_deref(), &parent)?;
    let local = handle.local().clone();
    let max_bytes = state.cfg.git.max_commit_bytes.as_u64();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_batch_commit(local.path(), request, &parent, max_bytes)
    })
    .await
    .map_err(|error| ApiError::Internal(format!("commit preparation task: {error}")))??;
    let pack = pack_commit_objects(
        &state,
        handle.local(),
        &prepared.commit_oid,
        &[prepared.parent_oid.as_str()],
    )
    .await?;
    drop(guard);

    let seq = publish_mutation(
        &handle,
        PreparedMutation {
            ref_name: ref_name.clone(),
            new_oid: prepared.commit_oid.clone(),
            new_peeled: String::new(),
            expected_old_oid: expected,
            pack: Some(pack),
            deleting: false,
        },
        mutation_meta(&headers, &principal),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CommitMutation {
            ref_name,
            oid: prepared.commit_oid.clone(),
            commit_oid: prepared.commit_oid,
            seq,
        }),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/{owner}/{repo}/api/merges",
    tag = "writes",
    summary = "Merge one revision into a branch",
    description = "Performs a policy-free Git merge. `merge` fast-forwards when possible and otherwise creates a two-parent commit; `squash` creates one commit with only the base parent; `fast-forward-only` never creates an object.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    request_body = MergeRequest,
    responses(
        (status = 201, description = "Merge applied", body = MergeMutation),
        (status = 200, description = "Head was already merged", body = MergeMutation),
        (status = 400, description = "Invalid branch, revision, identity, or timestamp"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Write access denied"),
        (status = 404, description = "Repository, branch, or head not found"),
        (status = 409, description = "CAS failure, non-fast-forward, or merge conflicts", body = MergeConflictResponse),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub(crate) async fn merge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo)): Path<(String, String)>,
    Json(request): Json<MergeRequest>,
) -> Result<Response, ApiError> {
    validate_identity(&request.committer, "committer")?;
    validate_expected(Some(&request.expected_base_oid))?;
    let expected = request.expected_base_oid.to_ascii_lowercase();
    let (handle, principal) = open_write(&state, &headers, &owner, &repo).await?;
    let ref_name = qualify_ref("heads", &request.base)?;
    let guard = handle.sync_full().await?;
    let base_oid = current_oid(&handle, &ref_name)
        .await?
        .ok_or_else(|| ApiError::NotFound(ref_name.clone()))?;
    ensure_expected(Some(&expected), &base_oid)?;
    let head_oid = super::write::resolve_commit_target(handle.local(), &request.head).await?;
    let local = handle.local().clone();
    let base_for_graph = base_oid.clone();
    let head_for_graph = head_oid.clone();
    let relation = tokio::task::spawn_blocking(move || {
        Ok::<_, ApiError>(MergeRelation {
            already_merged: is_ancestor(local.path(), &head_for_graph, &base_for_graph)?,
            can_fast_forward: is_ancestor(local.path(), &base_for_graph, &head_for_graph)?,
        })
    })
    .await
    .map_err(|error| ApiError::Internal(format!("merge relation task: {error}")))??;

    if relation.already_merged {
        return Ok(Json(MergeMutation {
            ref_name,
            oid: base_oid,
            commit_oid: None,
            seq: 0,
            already_merged: true,
        })
        .into_response());
    }

    if matches!(request.strategy, MergeStrategy::FastForwardOnly) && !relation.can_fast_forward {
        return Err(ApiError::Conflict("merge is not a fast-forward".into()));
    }

    let (new_oid, commit_oid, pack) = if relation.can_fast_forward
        && matches!(
            request.strategy,
            MergeStrategy::Merge | MergeStrategy::FastForwardOnly
        ) {
        (head_oid.clone(), None, None)
    } else {
        let local = handle.local().clone();
        let base_for_merge = base_oid.clone();
        let head_for_merge = head_oid.clone();
        let strategy = request.strategy;
        let message = request.message;
        let committer = request.committer;
        let outcome = tokio::task::spawn_blocking(move || {
            prepare_merge_commit(
                local.path(),
                &base_for_merge,
                &head_for_merge,
                strategy,
                &message,
                &committer,
            )
        })
        .await
        .map_err(|error| ApiError::Internal(format!("merge preparation task: {error}")))??;
        let commit = match outcome {
            PreparedMerge::Commit(commit) => commit,
            PreparedMerge::Conflicts(conflicts) => {
                return Ok((
                    StatusCode::CONFLICT,
                    Json(MergeConflictResponse {
                        error: "merge_conflict",
                        conflicts,
                    }),
                )
                    .into_response());
            }
        };
        let pack = pack_commit_objects(
            &state,
            handle.local(),
            &commit,
            &[base_oid.as_str(), head_oid.as_str()],
        )
        .await?;
        (commit.clone(), Some(commit), Some(pack))
    };
    drop(guard);

    let seq = publish_mutation(
        &handle,
        PreparedMutation {
            ref_name: ref_name.clone(),
            new_oid: new_oid.clone(),
            new_peeled: String::new(),
            expected_old_oid: Some(expected),
            pack,
            deleting: false,
        },
        mutation_meta(&headers, &principal),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(MergeMutation {
            ref_name,
            oid: new_oid,
            commit_oid,
            seq,
            already_merged: false,
        }),
    )
        .into_response())
}

pub(super) fn request_body_limit(config: &gitcask_config::Config) -> usize {
    let decoded = config.git.max_commit_bytes.as_u64();
    let encoded = decoded
        .saturating_add(2)
        .saturating_div(3)
        .saturating_mul(4);
    let metadata = (config.git.max_commit_changes as u64)
        .saturating_mul(64 * 1024)
        .saturating_add(1024 * 1024);
    usize::try_from(encoded.saturating_add(metadata)).unwrap_or(usize::MAX)
}

fn validate_commit_request(state: &AppState, request: &CommitRequest) -> Result<(), ApiError> {
    if request.changes.is_empty() {
        return Err(ApiError::BadRequest("changes must not be empty".into()));
    }
    if request.changes.len() > state.cfg.git.max_commit_changes {
        return Err(ApiError::PayloadTooLarge);
    }
    validate_identity(&request.committer, "committer")?;
    if let Some(author) = &request.author {
        validate_identity(author, "author")?;
    }
    Ok(())
}

fn validate_identity(identity: &CommitIdentity, field: &str) -> Result<(), ApiError> {
    let invalid_name = identity.name.is_empty()
        || identity
            .name
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'<' | b'>'));
    let invalid_email = identity.email.is_empty()
        || identity
            .email
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r' | b'<' | b'>'));
    if invalid_name || invalid_email {
        return Err(ApiError::BadRequest(format!("invalid {field} identity")));
    }
    chrono::DateTime::parse_from_rfc3339(&identity.when)
        .map_err(|_| ApiError::BadRequest(format!("{field}.when must be RFC 3339")))?;
    Ok(())
}

fn ensure_expected(expected: Option<&str>, actual: &str) -> Result<(), ApiError> {
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(ApiError::Conflict(format!(
            "expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

struct PreparedCommit {
    commit_oid: String,
    parent_oid: String,
}

#[derive(Clone)]
struct TreeEntry {
    mode: String,
    kind: String,
    oid: String,
}

enum LeafEdit {
    Set(TreeEntry),
    Delete,
}

#[derive(Default)]
struct TreeEdits {
    leaves: BTreeMap<Vec<u8>, LeafEdit>,
    dirs: BTreeMap<Vec<u8>, TreeEdits>,
}

type TreeEntries = BTreeMap<Vec<u8>, TreeEntry>;
type TreeCache = HashMap<String, TreeEntries>;

fn prepare_batch_commit(
    repo: &FsPath,
    request: CommitRequest,
    parent: &str,
    max_bytes: u64,
) -> Result<PreparedCommit, ApiError> {
    let parent_tree = rev_parse_tree(repo, parent)?;
    let mut cache = TreeCache::new();
    let mut edits = TreeEdits::default();
    let mut paths = HashSet::new();
    let mut decoded_bytes = 0_u64;

    for change in request.changes {
        match change {
            CommitChange::Upsert {
                path,
                content,
                mode,
            } => {
                let components = reserve_path(&mut paths, &path)?;
                if !matches!(mode.as_str(), "100644" | "100755" | "120000") {
                    return Err(ApiError::BadRequest(format!(
                        "invalid mode {mode} for {path}"
                    )));
                }
                if lookup_entry(repo, &parent_tree, &components, &mut cache)?
                    .is_some_and(|entry| entry.kind == "tree")
                {
                    return Err(ApiError::BadRequest(format!(
                        "cannot replace directory path {path} with a file"
                    )));
                }
                let content = base64::engine::general_purpose::STANDARD
                    .decode(content)
                    .map_err(|_| {
                        ApiError::BadRequest(format!("invalid base64 content for {path}"))
                    })?;
                decoded_bytes = decoded_bytes
                    .checked_add(content.len() as u64)
                    .ok_or(ApiError::PayloadTooLarge)?;
                if decoded_bytes > max_bytes {
                    return Err(ApiError::PayloadTooLarge);
                }
                let oid = hash_blob(repo, &content)?;
                insert_edit(
                    &mut edits,
                    &components,
                    LeafEdit::Set(TreeEntry {
                        mode,
                        kind: "blob".into(),
                        oid,
                    }),
                    &path,
                )?;
            }
            CommitChange::Delete { path } => {
                let components = reserve_path(&mut paths, &path)?;
                let entry = lookup_entry(repo, &parent_tree, &components, &mut cache)?.ok_or_else(
                    || ApiError::BadRequest(format!("cannot delete missing path {path}")),
                )?;
                if entry.kind == "tree" {
                    return Err(ApiError::BadRequest(format!(
                        "cannot delete directory path {path}"
                    )));
                }
                insert_edit(&mut edits, &components, LeafEdit::Delete, &path)?;
            }
            CommitChange::Rename { from, to } => {
                let from_components = reserve_path(&mut paths, &from)?;
                let to_components = reserve_path(&mut paths, &to)?;
                let entry = lookup_entry(repo, &parent_tree, &from_components, &mut cache)?
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!("cannot rename missing path {from}"))
                    })?;
                if entry.kind == "tree" {
                    return Err(ApiError::BadRequest(format!(
                        "cannot rename directory path {from}"
                    )));
                }
                if lookup_entry(repo, &parent_tree, &to_components, &mut cache)?.is_some() {
                    return Err(ApiError::BadRequest(format!(
                        "rename destination already exists: {to}"
                    )));
                }
                insert_edit(&mut edits, &from_components, LeafEdit::Delete, &from)?;
                insert_edit(&mut edits, &to_components, LeafEdit::Set(entry), &to)?;
            }
        }
    }

    let tree_oid = rewrite_tree(repo, Some(&parent_tree), &edits, &mut cache, true)?
        .ok_or_else(|| ApiError::Internal("root tree rewrite returned no tree".into()))?;
    if tree_oid == parent_tree && !request.allow_empty {
        return Err(ApiError::BadRequest(
            "changes produce the same tree; set allow_empty to create an empty commit".into(),
        ));
    }
    let author = request.author.as_ref().unwrap_or(&request.committer);
    let commit_oid = commit_tree(
        repo,
        &tree_oid,
        &[parent],
        &request.message,
        author,
        &request.committer,
    )?;
    Ok(PreparedCommit {
        commit_oid,
        parent_oid: parent.to_string(),
    })
}

fn reserve_path(seen: &mut HashSet<String>, path: &str) -> Result<Vec<Vec<u8>>, ApiError> {
    let components = validate_path(path)?;
    if !seen.insert(path.to_string()) {
        return Err(ApiError::BadRequest(format!(
            "path appears more than once: {path}"
        )));
    }
    Ok(components)
}

fn validate_path(path: &str) -> Result<Vec<Vec<u8>>, ApiError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.len() > MAX_PATH_BYTES
        || path.as_bytes().contains(&0)
    {
        return Err(ApiError::BadRequest(format!("invalid path {path:?}")));
    }
    let components: Vec<Vec<u8>> = path
        .split('/')
        .map(|component| component.as_bytes().to_vec())
        .collect();
    if components.iter().any(|component| {
        component.is_empty() || component.as_slice() == b"." || component.as_slice() == b".."
    }) {
        return Err(ApiError::BadRequest(format!("invalid path {path:?}")));
    }
    Ok(components)
}

fn insert_edit(
    node: &mut TreeEdits,
    components: &[Vec<u8>],
    edit: LeafEdit,
    original: &str,
) -> Result<(), ApiError> {
    let Some((name, rest)) = components.split_first() else {
        return Err(ApiError::BadRequest(format!("invalid path {original:?}")));
    };
    if rest.is_empty() {
        if node.dirs.contains_key(name) || node.leaves.insert(name.clone(), edit).is_some() {
            return Err(ApiError::BadRequest(format!(
                "overlapping path changes include {original}"
            )));
        }
        return Ok(());
    }
    if node.leaves.contains_key(name) {
        return Err(ApiError::BadRequest(format!(
            "overlapping path changes include {original}"
        )));
    }
    insert_edit(
        node.dirs.entry(name.clone()).or_default(),
        rest,
        edit,
        original,
    )
}

fn lookup_entry(
    repo: &FsPath,
    root_tree: &str,
    components: &[Vec<u8>],
    cache: &mut TreeCache,
) -> Result<Option<TreeEntry>, ApiError> {
    let mut tree_oid = root_tree.to_string();
    for (index, component) in components.iter().enumerate() {
        let entries = load_tree(repo, &tree_oid, cache)?;
        let Some(entry) = entries.get(component).cloned() else {
            return Ok(None);
        };
        if index + 1 == components.len() {
            return Ok(Some(entry));
        }
        if entry.kind != "tree" {
            return Ok(None);
        }
        tree_oid = entry.oid;
    }
    Ok(None)
}

fn load_tree<'a>(
    repo: &FsPath,
    oid: &str,
    cache: &'a mut TreeCache,
) -> Result<&'a TreeEntries, ApiError> {
    if !cache.contains_key(oid) {
        let output = run_git(repo, &["ls-tree", "-z", oid], &[], None)?;
        ensure_git_success(output.status, &output.stderr, "git ls-tree")?;
        let mut entries = TreeEntries::new();
        for record in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|r| !r.is_empty())
        {
            let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
                return Err(ApiError::Internal(
                    "git ls-tree returned an invalid record".into(),
                ));
            };
            let Some((header_bytes, name)) = record
                .get(..tab)
                .zip(tab.checked_add(1).and_then(|start| record.get(start..)))
            else {
                return Err(ApiError::Internal(
                    "git ls-tree returned an invalid record".into(),
                ));
            };
            let header = std::str::from_utf8(header_bytes).map_err(|_| {
                ApiError::Internal("git ls-tree returned a non-ASCII header".into())
            })?;
            let mut fields = header.split_whitespace();
            let mode = fields.next().unwrap_or_default().to_string();
            let kind = fields.next().unwrap_or_default().to_string();
            let object = fields.next().unwrap_or_default().to_string();
            gitcask_git::validate_oid(&object)?;
            entries.insert(
                name.to_vec(),
                TreeEntry {
                    mode,
                    kind,
                    oid: object,
                },
            );
        }
        cache.insert(oid.to_string(), entries);
    }
    cache
        .get(oid)
        .ok_or_else(|| ApiError::Internal("tree cache insertion failed".into()))
}

fn rewrite_tree(
    repo: &FsPath,
    base_oid: Option<&str>,
    edits: &TreeEdits,
    cache: &mut TreeCache,
    root: bool,
) -> Result<Option<String>, ApiError> {
    let mut entries = match base_oid {
        Some(oid) => load_tree(repo, oid, cache)?.clone(),
        None => TreeEntries::new(),
    };
    for (name, edit) in &edits.leaves {
        match edit {
            LeafEdit::Set(entry) => {
                entries.insert(name.clone(), entry.clone());
            }
            LeafEdit::Delete => {
                entries.remove(name);
            }
        }
    }
    for (name, child_edits) in &edits.dirs {
        let child_base = match entries.get(name) {
            Some(entry) if entry.kind == "tree" => Some(entry.oid.clone()),
            Some(_) => {
                return Err(ApiError::BadRequest(format!(
                    "path component is not a directory: {}",
                    String::from_utf8_lossy(name)
                )));
            }
            None => None,
        };
        match rewrite_tree(repo, child_base.as_deref(), child_edits, cache, false)? {
            Some(oid) => {
                entries.insert(
                    name.clone(),
                    TreeEntry {
                        mode: "040000".into(),
                        kind: "tree".into(),
                        oid,
                    },
                );
            }
            None => {
                entries.remove(name);
            }
        }
    }
    if entries.is_empty() && !root {
        return Ok(None);
    }
    Ok(Some(mktree(repo, &entries)?))
}

fn hash_blob(repo: &FsPath, content: &[u8]) -> Result<String, ApiError> {
    let output = run_git(repo, &["hash-object", "-w", "--stdin"], &[], Some(content))?;
    ensure_git_success(output.status, &output.stderr, "git hash-object")?;
    parse_oid(&output.stdout, "git hash-object")
}

fn rev_parse_tree(repo: &FsPath, commit: &str) -> Result<String, ApiError> {
    let expression = format!("{commit}^{{tree}}");
    let output = run_git(
        repo,
        &["rev-parse", "--verify", "--end-of-options", &expression],
        &[],
        None,
    )?;
    ensure_git_success(output.status, &output.stderr, "git rev-parse tree")?;
    parse_oid(&output.stdout, "git rev-parse tree")
}

fn mktree(repo: &FsPath, entries: &TreeEntries) -> Result<String, ApiError> {
    let mut input = Vec::new();
    for (name, entry) in entries {
        input
            .extend_from_slice(format!("{} {} {}\t", entry.mode, entry.kind, entry.oid).as_bytes());
        input.extend_from_slice(name);
        input.push(0);
    }
    let output = run_git(repo, &["mktree", "-z"], &[], Some(&input))?;
    ensure_git_success(output.status, &output.stderr, "git mktree")?;
    parse_oid(&output.stdout, "git mktree")
}

fn commit_tree(
    repo: &FsPath,
    tree: &str,
    parents: &[&str],
    message: &str,
    author: &CommitIdentity,
    committer: &CommitIdentity,
) -> Result<String, ApiError> {
    let mut owned_args = vec!["commit-tree".to_string(), tree.to_string()];
    for parent in parents {
        owned_args.push("-p".into());
        owned_args.push((*parent).to_string());
    }
    owned_args.extend(["-F".into(), "-".into()]);
    let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let author_date = git_identity_time(&author.when)?;
    let committer_date = git_identity_time(&committer.when)?;
    let env = [
        ("GIT_AUTHOR_NAME", author.name.as_str()),
        ("GIT_AUTHOR_EMAIL", author.email.as_str()),
        ("GIT_AUTHOR_DATE", author_date.as_str()),
        ("GIT_COMMITTER_NAME", committer.name.as_str()),
        ("GIT_COMMITTER_EMAIL", committer.email.as_str()),
        ("GIT_COMMITTER_DATE", committer_date.as_str()),
    ];
    let output = run_git(repo, &args, &env, Some(message.as_bytes()))?;
    ensure_git_success(output.status, &output.stderr, "git commit-tree")?;
    parse_oid(&output.stdout, "git commit-tree")
}

fn git_identity_time(when: &str) -> Result<String, ApiError> {
    let parsed = chrono::DateTime::parse_from_rfc3339(when)
        .map_err(|_| ApiError::BadRequest("identity timestamp must be RFC 3339".into()))?;
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

struct MergeRelation {
    already_merged: bool,
    can_fast_forward: bool,
}

enum PreparedMerge {
    Commit(String),
    Conflicts(Vec<String>),
}

fn is_ancestor(repo: &FsPath, ancestor: &str, descendant: &str) -> Result<bool, ApiError> {
    let output = run_git(
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        &[],
        None,
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            ensure_git_success(
                output.status,
                &output.stderr,
                "git merge-base --is-ancestor",
            )?;
            Ok(false)
        }
    }
}

fn prepare_merge_commit(
    repo: &FsPath,
    base: &str,
    head: &str,
    strategy: MergeStrategy,
    message: &str,
    committer: &CommitIdentity,
) -> Result<PreparedMerge, ApiError> {
    let output = run_git(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            "--no-messages",
            base,
            head,
        ],
        &[],
        None,
    )?;
    let mut records = output.stdout.split(|byte| *byte == 0);
    let tree = records
        .next()
        .map(trim_ascii)
        .filter(|record| !record.is_empty())
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "git merge-tree returned no tree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        })?;
    let tree = std::str::from_utf8(tree)
        .map_err(|_| ApiError::Internal("git merge-tree returned a non-ASCII oid".into()))?;
    gitcask_git::validate_oid(tree)?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => {
            let mut conflicts: Vec<String> = records
                .map(trim_ascii)
                .filter(|record| !record.is_empty())
                .map(|record| String::from_utf8_lossy(record).into_owned())
                .collect();
            conflicts.sort();
            conflicts.dedup();
            return Ok(PreparedMerge::Conflicts(conflicts));
        }
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.contains("unrelated histories") {
                return Err(ApiError::Conflict("histories are unrelated".into()));
            }
            return Err(ApiError::Internal(format!(
                "git merge-tree exited {:?}: {stderr}",
                output.status.code()
            )));
        }
    }
    let parents = match strategy {
        MergeStrategy::Merge => vec![base, head],
        MergeStrategy::Squash => vec![base],
        MergeStrategy::FastForwardOnly => {
            return Err(ApiError::Internal(
                "fast-forward-only merge unexpectedly required a commit".into(),
            ));
        }
    };
    let commit = commit_tree(repo, tree, &parents, message, committer, committer)?;
    Ok(PreparedMerge::Commit(commit))
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .and_then(|index| index.checked_add(1))
        .unwrap_or(start);
    bytes.get(start..end).unwrap_or_default()
}

fn parse_oid(bytes: &[u8], command: &str) -> Result<String, ApiError> {
    let oid = String::from_utf8_lossy(bytes).trim().to_string();
    gitcask_git::validate_oid(&oid)
        .map_err(|_| ApiError::Internal(format!("{command} returned invalid oid {oid:?}")))?;
    Ok(oid)
}

fn ensure_git_success(
    status: std::process::ExitStatus,
    stderr: &[u8],
    command: &str,
) -> Result<(), ApiError> {
    if status.success() {
        return Ok(());
    }
    Err(ApiError::Internal(format!(
        "{command} exited {:?}: {}",
        status.code(),
        String::from_utf8_lossy(stderr).trim()
    )))
}

fn run_git(
    repo: &FsPath,
    args: &[&str],
    env: &[(&str, &str)],
    input: Option<&[u8]>,
) -> Result<std::process::Output, ApiError> {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo)
        .env("GIT_DIR", repo)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| ApiError::Internal(format!("git {}: {error}", args.join(" "))))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .ok_or_else(|| ApiError::Internal("git stdin unavailable".into()))?
            .write_all(input)
            .map_err(|error| ApiError::Internal(format!("git stdin: {error}")))?;
    }
    child
        .wait_with_output()
        .map_err(|error| ApiError::Internal(format!("git {}: {error}", args.join(" "))))
}

async fn pack_commit_objects(
    state: &AppState,
    local: &gitcask_git::LocalRepo,
    commit: &str,
    excludes: &[&str],
) -> Result<gitcask_git::IngestedPack, ApiError> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(local.path())
        .env("GIT_DIR", local.path())
        .args(["pack-objects", "--stdout", "--revs"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let mut input = format!("{commit}\n");
    for exclude in excludes {
        input.push('^');
        input.push_str(exclude);
        input.push('\n');
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_reject_duplicates_and_overlaps() {
        let mut edits = TreeEdits::default();
        let leaf = || {
            LeafEdit::Set(TreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                oid: "0".repeat(40),
            })
        };
        insert_edit(&mut edits, &validate_path("a").unwrap(), leaf(), "a").unwrap();
        assert!(insert_edit(&mut edits, &validate_path("a/b").unwrap(), leaf(), "a/b").is_err());
        assert!(validate_path("../outside").is_err());
        assert!(validate_path("a//b").is_err());
    }
}
