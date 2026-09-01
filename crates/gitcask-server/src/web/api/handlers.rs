//! Axum handlers for the repository browsing API.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::Response,
};

use crate::{AppState, error::ApiError, sse::Rendered};

use super::git::{
    GitFailure, bounded_patch, git, log_format, parse_commits, parse_compare_counts,
    parse_compare_files, parse_stats,
};
use super::view::{
    IMMUTABLE_CACHE_CONTROL, Need, Repo, SWR_CACHE_CONTROL, etag_for, finish, json_bytes, json_swr,
    open, run, view,
};
use super::{
    Blob, BlobQuery, CommitDetail, CommitQuery, Commits, Compare, CompareRef, Readme, RefInfo,
    RefListQuery, RefPage, Refs, Resolved, Tree, TreeEntry,
};

const MAX_BLOB: usize = 2 * 1024 * 1024;
const MAX_COMPARE_COMMITS: usize = 250;
const DEFAULT_PAGE: usize = 100;
const MAX_PAGE: usize = 1000;

fn not_found(message: impl Into<String>) -> ApiError {
    ApiError::NotFound(message.into())
}

fn is_full_sha(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// ---- refs --------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/refs",
    tag = "browsing",
    summary = "Get the default ref",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 200, description = "Default ref", body = Refs),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn refs(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Refs,
        None,
        |r| async move {
            let head = r.index.head().map(|(name, sha)| RefInfo { name, sha });
            let etag = etag_for(head.as_ref().map(|h| h.sha.as_str()).unwrap_or("unborn"));
            Ok(json_swr(&Refs { head }, Some(&etag)))
        },
    )
    .await
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/refs/{kind}",
    tag = "browsing",
    summary = "List branches or tags",
    description = "Returns a byte-sorted, prefix-filtered page. Clients accepting `text/event-stream` receive the same page as SSE packets.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("kind" = String, Path, description = "Ref namespace: `branches` or `tags`"),
        RefListQuery
    ),
    responses(
        (status = 200, description = "Page of refs", body = RefPage),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or ref namespace not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn ref_list(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, kind)): Path<(String, String, String)>,
    Query(q): Query<RefListQuery>,
) -> Result<Response, ApiError> {
    let wants_sse = crate::sse::wants_sse(&headers);
    let handle = open(&st, &headers, &owner, &repo_name).await?;
    let r = view(&st, handle, Need::Refs).await?;
    let list = match kind.as_str() {
        "branches" => &r.index.branches,
        "tags" => &r.index.tags,
        _ => return Err(not_found("ref namespace")),
    };
    let n = q.n.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let prefix = q
        .prefix
        .as_deref()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}/"));
    let needle =
        q.q.as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());
    let after = q.after.as_deref().unwrap_or("");
    // Byte-sorted: skip straight to the first candidate (> after, >= prefix).
    let lower = match &prefix {
        Some(p) if p.as_str() > after => p.as_str(),
        _ => after,
    };
    let start = list
        .partition_point(|(name, _)| name.as_str() <= lower && name.as_str() != lower)
        .max(list.partition_point(|(name, _)| name.as_str() <= after));
    let mut refs = Vec::with_capacity(n.min(256));
    let mut more = false;
    for (name, sha) in &list[start..] {
        if let Some(p) = &prefix {
            if !name.starts_with(p.as_str()) {
                break; // sorted: no further names share the prefix
            }
        }
        if let Some(nd) = &needle {
            if !name.to_ascii_lowercase().contains(nd.as_str()) {
                continue;
            }
        }
        if refs.len() == n {
            more = true;
            break;
        }
        refs.push(RefInfo {
            name: name.clone(),
            sha: sha.clone(),
        });
    }
    if wants_sse {
        // Streamed form: one `ref` packet per ref, then `done`.
        let mut items: Vec<Result<bytes::Bytes, std::convert::Infallible>> =
            Vec::with_capacity(refs.len() + 1);
        for r in &refs {
            items.push(Ok(crate::sse::packet("ref", r)));
        }
        items.push(Ok(crate::sse::packet(
            "done",
            &serde_json::json!({ "more": more }),
        )));
        let mut resp = crate::sse::sse_response(futures::stream::iter(items));
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, SWR_CACHE_CONTROL.parse().unwrap());
        return Ok(resp);
    }
    Ok(json_swr(&RefPage { refs, more }, None).into_response(&headers))
}

// ---- resolve -----------------------------------------------------------------

/// §3: longest branch/tag prefix of `rest` wins (branch beats tag on ties);
/// else the first segment must be a commit-ish; empty -> default branch.
async fn resolve_rest(r: &Repo, rest: &str) -> Result<Resolved, ApiError> {
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        let (name, sha) = r.index.head().ok_or_else(|| not_found("unborn HEAD"))?;
        return Ok(Resolved {
            ref_name: name,
            sha,
            path: String::new(),
            kind: "branch",
        });
    }
    // Candidate prefixes, longest first.
    let mut cut_points: Vec<usize> = rest.match_indices('/').map(|(i, _)| i).collect();
    cut_points.push(rest.len());
    for &cut in cut_points.iter().rev() {
        let name = &rest[..cut];
        let path = rest[cut..].trim_start_matches('/').to_string();
        if let Some(sha) = r.index.branch(name) {
            return Ok(Resolved {
                ref_name: name.to_string(),
                sha: sha.to_string(),
                path,
                kind: "branch",
            });
        }
        if let Some(sha) = r.index.tag(name) {
            return Ok(Resolved {
                ref_name: name.to_string(),
                sha: sha.to_string(),
                path,
                kind: "tag",
            });
        }
    }
    let (first, path) = match rest.split_once('/') {
        Some((f, p)) => (f, p.to_string()),
        None => (rest, String::new()),
    };
    let sha = rev_parse_commit(r, first).await?;
    Ok(Resolved {
        ref_name: first.to_string(),
        sha,
        path,
        kind: "commit",
    })
}

/// Resolve a single revision name (no path): branch, tag, then git rev-parse.
async fn resolve_name(r: &Repo, name: &str) -> Result<Resolved, ApiError> {
    if name.is_empty() || name == "HEAD" {
        if let Some((n, sha)) = r.index.head() {
            return Ok(Resolved {
                ref_name: n,
                sha,
                path: String::new(),
                kind: "branch",
            });
        }
    }
    if let Some(sha) = r.index.branch(name) {
        return Ok(Resolved {
            ref_name: name.into(),
            sha: sha.into(),
            path: String::new(),
            kind: "branch",
        });
    }
    if let Some(sha) = r.index.tag(name) {
        return Ok(Resolved {
            ref_name: name.into(),
            sha: sha.into(),
            path: String::new(),
            kind: "tag",
        });
    }
    let sha = rev_parse_commit(r, name).await?;
    Ok(Resolved {
        ref_name: name.into(),
        sha,
        path: String::new(),
        kind: "commit",
    })
}

/// `rev-parse --verify <rev>^{commit}` against the local object store.
async fn rev_parse_commit(r: &Repo, rev: &str) -> Result<String, ApiError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(not_found("revision"));
    }
    if !r.objects {
        return Err(not_found(format!("unknown revision {rev}")));
    }
    let out = git(
        &r.local,
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            "--end-of-options".into(),
            format!("{rev}^{{commit}}"),
        ],
        GitFailure::NotFound,
    )
    .await
    .map_err(|error| match error {
        ApiError::NotFound(_) => not_found(format!("unknown revision {rev}")),
        other => other,
    })?;
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if sha.is_empty() {
        return Err(not_found(format!("unknown revision {rev}")));
    }
    Ok(sha)
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/resolve",
    tag = "browsing",
    summary = "Resolve the default ref",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 200, description = "Resolved default ref", body = Resolved),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or default ref not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn resolve_root(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    resolve_impl(&st, &headers, &owner, &repo_name, "").await
}
#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/resolve/{rest}",
    tag = "browsing",
    summary = "Resolve a revision and optional path",
    description = "The longest branch or tag prefix wins; otherwise the first segment is resolved as a commit-ish.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("rest" = String, Path, description = "Revision followed by an optional repository path")
    ),
    responses(
        (status = 200, description = "Resolved revision and path", body = Resolved),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository, revision, or path not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn resolve(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    resolve_impl(&st, &headers, &owner, &repo_name, &rest).await
}
async fn resolve_impl(
    st: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    repo_name: &str,
    rest: &str,
) -> Result<Response, ApiError> {
    // Branch/tag names resolve from the index; a raw revision falls back to
    // object access. Refs-only first so huge repos still answer for named refs.
    let handle = open(st, headers, owner, repo_name).await?;
    let mut r = view(st, handle, Need::Refs).await?;
    let res = match resolve_rest(&r, rest).await {
        Ok(res) => res,
        Err(ApiError::NotFound(_)) if !r.objects => {
            r.need_objects(st).await?;
            resolve_rest(&r, rest).await?
        }
        Err(e) => return Err(e),
    };
    let etag = etag_for(&res.sha);
    Ok(json_swr(&res, Some(&etag)).into_response(headers))
}

/// Split `{ref}/{path}` for tree/blob: a leading full sha is taken verbatim
/// (immutable response); otherwise §3 resolution (SWR + ETag).
fn split_addr(rest: &str) -> Option<(Resolved, bool)> {
    let rest = rest.trim_matches('/');
    let (first, path) = match rest.split_once('/') {
        Some((f, p)) => (f, p.trim_matches('/').to_string()),
        None => (rest, String::new()),
    };
    is_full_sha(first).then(|| {
        (
            Resolved {
                ref_name: first.to_string(),
                sha: first.to_string(),
                path,
                kind: "commit",
            },
            true,
        )
    })
}
async fn resolve_addr(r: &Repo, rest: &str) -> Result<(Resolved, bool), ApiError> {
    if let Some(x) = split_addr(rest) {
        return Ok(x);
    }
    Ok((resolve_rest(r, rest).await?, false))
}

// ---- tree --------------------------------------------------------------------

fn tree_key(repo: &str, sha: &str, path: &str) -> String {
    format!("{repo}\0tree\0{sha}\0{path}")
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/tree/{rest}",
    tag = "browsing",
    summary = "Browse a tree",
    description = "Returns entries, the newest commit touching the path, and a UTF-8 README when present.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("rest" = String, Path, description = "Revision followed by an optional tree path")
    ),
    responses(
        (status = 200, description = "Tree contents", body = Tree),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository, revision, or tree not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn tree(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let key = split_addr(&rest)
        .map(|(res, _)| tree_key(&format!("{owner}/{repo_name}"), &res.sha, &res.path));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = resolve_addr(&r, &rest).await?;
            let key = tree_key(&r.id, &res.sha, &res.path);
            let body = render_tree(&r.local, &res).await?;
            Ok(finish(&st2, &r, immutable, &key, &res.sha, body))
        },
    )
    .await
}

async fn render_tree(
    local: &gitcask_git::LocalRepo,
    res: &Resolved,
) -> Result<bytes::Bytes, ApiError> {
    let spec = if res.path.is_empty() {
        format!("{}^{{tree}}", res.sha)
    } else {
        format!("{}:{}", res.sha, res.path)
    };
    let bytes = git(
        local,
        vec!["ls-tree".into(), "-l".into(), "-z".into(), spec],
        GitFailure::NotFound,
    )
    .await?;
    let mut entries = Vec::new();
    for item in bytes.split(|b| *b == 0).filter(|x| !x.is_empty()) {
        let Some(tab) = item.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let (meta, name) = item.split_at(tab);
        let name = &name[1..];
        // `ls-tree -l` right-aligns the size with padding spaces.
        let fields: Vec<&[u8]> = meta
            .split(|b| *b == b' ')
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() < 4 {
            continue;
        }
        let kind = String::from_utf8_lossy(fields[1]).to_string();
        let size = if kind == "blob" {
            String::from_utf8_lossy(fields[3]).parse().unwrap_or(-1)
        } else {
            -1
        };
        entries.push(TreeEntry {
            name: String::from_utf8_lossy(name).to_string(),
            kind,
            mode: String::from_utf8_lossy(fields[0]).to_string(),
            size,
            sha: String::from_utf8_lossy(fields[2]).to_string(),
        });
    }
    sort_entries(&mut entries);
    let commit = parse_commits(&newest_commit(local, &res.sha, &res.path).await?)
        .into_iter()
        .next();
    let mut readme = None;
    if let Some(e) = readme_entry(&entries) {
        let content = git(
            local,
            vec!["cat-file".into(), "blob".into(), e.sha.clone()],
            GitFailure::Internal,
        )
        .await?;
        if let Ok(contents) = String::from_utf8(content) {
            readme = Some(Readme {
                name: e.name.clone(),
                contents,
            });
        }
    }
    Ok(json_bytes(&Tree {
        ref_name: res.ref_name.clone(),
        sha: res.sha.clone(),
        path: res.path.clone(),
        entries,
        commit,
        readme,
    }))
}

fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        let ad = a.kind == "tree";
        let bd = b.kind == "tree";
        bd.cmp(&ad)
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
}
fn readme_entry(entries: &[TreeEntry]) -> Option<&TreeEntry> {
    entries.iter().find(|e| {
        e.kind == "blob"
            && [
                "readme",
                "readme.md",
                "readme.markdown",
                "readme.txt",
                "readme.rst",
            ]
            .contains(&e.name.to_ascii_lowercase().as_str())
    })
}

async fn newest_commit(
    local: &gitcask_git::LocalRepo,
    sha: &str,
    path: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut a = vec![
        "log".into(),
        "-1".into(),
        format!("--format={}", log_format()),
        sha.into(),
    ];
    if !path.is_empty() {
        a.push("--".into());
        a.push(path.into());
    }
    git(local, a, GitFailure::Internal).await
}

// ---- blob ----------------------------------------------------------------------

fn blob_key(repo: &str, sha: &str, path: &str) -> String {
    format!("{repo}\0blob\0{sha}\0{path}")
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/blob/{rest}",
    tag = "browsing",
    summary = "Read a blob",
    description = "Returns UTF-8 contents inline up to 2 MiB. `raw` returns a text blob as `text/plain`.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("rest" = String, Path, description = "Revision followed by the blob path"),
        BlobQuery
    ),
    responses(
        (status = 200, description = "Blob metadata and optional contents", body = Blob),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository, revision, or blob not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn blob(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
    Query(q): Query<BlobQuery>,
) -> Result<Response, ApiError> {
    let raw = q.raw.is_some();
    // `?raw` is a page navigation (the "Raw" link): never the SSE envelope.
    let mut plain_headers = headers.clone();
    if raw {
        plain_headers.remove(header::ACCEPT);
    }
    let key = if raw {
        None
    } else {
        split_addr(&rest)
            .map(|(res, _)| blob_key(&format!("{owner}/{repo_name}"), &res.sha, &res.path))
    };
    let st2 = st.clone();
    run(
        &st,
        &plain_headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = resolve_addr(&r, &rest).await?;
            if res.path.is_empty() {
                return Err(not_found("blob path"));
            }
            let name = res.path.rsplit('/').next().unwrap_or(&res.path).to_string();
            let bytes = git(
                &r.local,
                vec![
                    "cat-file".into(),
                    "blob".into(),
                    format!("{}:{}", res.sha, res.path),
                ],
                GitFailure::NotFound,
            )
            .await?;
            let (size, bytes) = (bytes.len() as i64, Some(bytes));
            let is_text = size <= MAX_BLOB as i64
                && bytes
                    .as_ref()
                    .map(|b| !b.contains(&0) && std::str::from_utf8(b).is_ok())
                    .unwrap_or(false);
            if raw && is_text {
                let etag = etag_for(&res.sha);
                return Ok(Rendered {
                    body: bytes::Bytes::from(bytes.unwrap_or_default()),
                    content_type: "text/plain; charset=utf-8",
                    cache_control: if immutable {
                        IMMUTABLE_CACHE_CONTROL
                    } else {
                        SWR_CACHE_CONTROL
                    },
                    etag: (!immutable).then_some(etag),
                });
            }
            let b = if size > MAX_BLOB as i64 {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: None,
                    binary: None,
                    too_large: Some(true),
                }
            } else if !is_text {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: None,
                    binary: Some(true),
                    too_large: None,
                }
            } else {
                Blob {
                    ref_name: res.ref_name.clone(),
                    sha: res.sha.clone(),
                    path: res.path.clone(),
                    name,
                    size,
                    contents: Some(
                        String::from_utf8(bytes.unwrap_or_default()).unwrap_or_default(),
                    ),
                    binary: None,
                    too_large: None,
                }
            };
            let key = blob_key(&r.id, &res.sha, &res.path);
            Ok(finish(&st2, &r, immutable, &key, &res.sha, json_bytes(&b)))
        },
    )
    .await
}

// ---- commits -------------------------------------------------------------------

fn commits_key(repo: &str, sha: &str, path: &str, skip: usize, n: usize) -> String {
    format!("{repo}\0commits\0{sha}\0{path}\0{skip}\0{n}")
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/commits",
    tag = "browsing",
    summary = "List commits",
    description = "Returns a bounded page of commits, optionally restricted to a repository path.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        CommitQuery
    ),
    responses(
        (status = 200, description = "Commit page", body = Commits),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or revision not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn commits(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name)): Path<(String, String)>,
    Query(q): Query<CommitQuery>,
) -> Result<Response, ApiError> {
    let reference = q.ref_.clone().unwrap_or_else(|| "HEAD".into());
    let skip = q.skip.unwrap_or(0);
    let n = q.n.unwrap_or(35).clamp(1, 200);
    let path = q.path.clone().unwrap_or_default();
    let key = is_full_sha(&reference)
        .then(|| commits_key(&format!("{owner}/{repo_name}"), &reference, &path, skip, n));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let (res, immutable) = if is_full_sha(&reference) {
                (
                    Resolved {
                        ref_name: reference.clone(),
                        sha: reference.clone(),
                        path: String::new(),
                        kind: "commit",
                    },
                    true,
                )
            } else {
                (resolve_name(&r, &reference).await?, false)
            };
            let key = commits_key(&r.id, &res.sha, &path, skip, n);
            let mut a = vec![
                "log".into(),
                format!("--format={}", log_format()),
                "--no-color".into(),
                format!("--skip={skip}"),
                format!("-{count}", count = n.saturating_add(1)),
                res.sha.clone(),
            ];
            if !path.is_empty() {
                a.extend(["--".into(), path.clone()]);
            }
            let mut cs = parse_commits(&git(&r.local, a, GitFailure::Internal).await?);
            let more = cs.len() > n;
            if more {
                cs.truncate(n);
            }
            let body = json_bytes(&Commits {
                ref_name: res.ref_name.clone(),
                sha: res.sha.clone(),
                commits: cs,
                more,
            });
            Ok(finish(&st2, &r, immutable, &key, &res.sha, body))
        },
    )
    .await
}

// ---- compare ------------------------------------------------------------------

fn compare_key(repo: &str, base: &str, head: &str) -> String {
    format!("{repo}\0compare\0{base}\0{head}")
}

fn split_compare(rest: &str) -> Result<(&str, &str), ApiError> {
    let Some((base, head)) = rest.split_once("...") else {
        return Err(not_found("compare head: missing `...` separator"));
    };
    if base.is_empty() {
        return Err(not_found("compare base: missing revision"));
    }
    if head.is_empty() {
        return Err(not_found("compare head: missing revision"));
    }
    Ok((base, head))
}

async fn resolve_compare_name(r: &Repo, name: &str, side: &str) -> Result<Resolved, ApiError> {
    resolve_name(r, name).await.map_err(|error| match error {
        ApiError::NotFound(message) => not_found(format!("compare {side} `{name}`: {message}")),
        other => other,
    })
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/compare/{base}...{head}",
    tag = "browsing",
    summary = "Compare two revisions",
    description = "Computes a merge-base comparison with bounded commits, file statistics, and patch output.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("base" = String, Path, description = "Base branch, tag, or commit"),
        ("head" = String, Path, description = "Head branch, tag, or commit")
    ),
    responses(
        (status = 200, description = "Merge-base comparison", body = Compare),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or revision not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn compare(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rest)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (base_ref, head_ref) = split_compare(&rest)?;
    let base_ref = base_ref.to_string();
    let head_ref = head_ref.to_string();
    let immutable = is_full_sha(&base_ref) && is_full_sha(&head_ref);
    let key = immutable.then(|| compare_key(&format!("{owner}/{repo_name}"), &base_ref, &head_ref));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let base = resolve_compare_name(&r, &base_ref, "base").await?;
            let head = resolve_compare_name(&r, &head_ref, "head").await?;
            let key = compare_key(&r.id, &base.sha, &head.sha);

            let (merge_base_out, counts_out, commits_out) = tokio::try_join!(
                git(
                    &r.local,
                    vec!["merge-base".into(), base.sha.clone(), head.sha.clone()],
                    GitFailure::NotFound,
                ),
                git(
                    &r.local,
                    vec![
                        "rev-list".into(),
                        "--left-right".into(),
                        "--count".into(),
                        format!("{}...{}", base.sha, head.sha),
                    ],
                    GitFailure::Internal,
                ),
                git(
                    &r.local,
                    vec![
                        "log".into(),
                        format!("--format={}", log_format()),
                        "--no-color".into(),
                        format!("--max-count={}", MAX_COMPARE_COMMITS + 1),
                        format!("{}..{}", base.sha, head.sha),
                    ],
                    GitFailure::Internal,
                ),
            )?;
            let merge_base = String::from_utf8_lossy(&merge_base_out).trim().to_string();
            if merge_base.is_empty() {
                return Err(not_found("compare merge base"));
            }
            let (ahead_by, behind_by) = parse_compare_counts(&counts_out)?;
            let mut commits = parse_commits(&commits_out);
            let commits_truncated = commits.len() > MAX_COMPARE_COMMITS;
            if commits_truncated {
                commits.truncate(MAX_COMPARE_COMMITS);
            }

            let diff_range = format!("{merge_base}..{}", head.sha);
            let (stats_out, statuses_out, patch_out) = tokio::try_join!(
                git(
                    &r.local,
                    vec![
                        "diff".into(),
                        "--numstat".into(),
                        "-M".into(),
                        "--no-ext-diff".into(),
                        diff_range.clone(),
                    ],
                    GitFailure::Internal,
                ),
                git(
                    &r.local,
                    vec![
                        "diff".into(),
                        "--name-status".into(),
                        "-M".into(),
                        "--no-ext-diff".into(),
                        diff_range.clone(),
                    ],
                    GitFailure::Internal,
                ),
                git(
                    &r.local,
                    vec![
                        "diff".into(),
                        "-p".into(),
                        "-M".into(),
                        "--no-color".into(),
                        "--no-ext-diff".into(),
                        diff_range,
                    ],
                    GitFailure::Internal,
                ),
            )?;
            let files = parse_compare_files(&statuses_out, &stats_out);
            let (patch, patch_truncated) = bounded_patch(&patch_out);
            let etag = format!("{}-{}", base.sha, head.sha);
            let body = json_bytes(&Compare {
                base: CompareRef {
                    ref_name: base_ref,
                    sha: base.sha,
                },
                head: CompareRef {
                    ref_name: head_ref,
                    sha: head.sha,
                },
                merge_base,
                ahead_by,
                behind_by,
                commits,
                files,
                patch,
                truncated: commits_truncated || patch_truncated,
            });
            Ok(finish(&st2, &r, immutable, &key, &etag, body))
        },
    )
    .await
}

// ---- commit detail -------------------------------------------------------------

fn commit_key(repo: &str, sha: &str) -> String {
    format!("{repo}\0commit\0{sha}")
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/commit/{sha}",
    tag = "browsing",
    summary = "Get commit details",
    description = "Returns commit metadata, first-parent file statistics, and a bounded patch.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("sha" = String, Path, description = "Commit id or commit-ish")
    ),
    responses(
        (status = 200, description = "Commit details", body = CommitDetail),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or commit not found"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn commit_detail(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo_name, rev)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let key = is_full_sha(&rev).then(|| commit_key(&format!("{owner}/{repo_name}"), &rev));
    let st2 = st.clone();
    run(
        &st,
        &headers,
        &owner,
        &repo_name,
        Need::Objects,
        key,
        move |r| async move {
            let immutable = is_full_sha(&rev);
            let sha = if immutable {
                rev.clone()
            } else {
                resolve_name(&r, &rev).await?.sha
            };
            let key = commit_key(&r.id, &sha);
            let commit = parse_commits(
                // `--diff-merges=off`: avoid setting up a combined diff for a
                // merge when only its metadata is requested.
                &git(
                    &r.local,
                    vec![
                        "show".into(),
                        "-s".into(),
                        "--diff-merges=off".into(),
                        format!("--format={}", log_format()),
                        sha.clone(),
                    ],
                    GitFailure::Internal,
                )
                .await?,
            )
            .into_iter()
            .next()
            .ok_or_else(|| not_found("commit"))?;
            let stat_out = git(
                &r.local,
                vec![
                    "show".into(),
                    "--format=".into(),
                    "--numstat".into(),
                    "-M".into(),
                    "--diff-merges=first-parent".into(),
                    "--root".into(),
                    sha.clone(),
                ],
                GitFailure::Internal,
            )
            .await?;
            let stats = parse_stats(&stat_out);
            let patch_out = git(
                &r.local,
                vec![
                    "show".into(),
                    "--format=".into(),
                    "-p".into(),
                    "-M".into(),
                    "--no-color".into(),
                    "--no-ext-diff".into(),
                    "--diff-merges=first-parent".into(),
                    "--root".into(),
                    sha.clone(),
                ],
                GitFailure::Internal,
            )
            .await?;
            let (patch, _) = bounded_patch(&patch_out);
            let body = json_bytes(&CommitDetail {
                commit,
                stats,
                patch,
            });
            Ok(finish(&st2, &r, immutable, &key, &sha, body))
        },
    )
    .await
}
