//! JSON API for reading repository contents and performing deterministic Git
//! mutations without a checkout.
//!
//! Two URL classes: ref-dependent (`refs`, `refs/{branches,tags}`,
//! `resolve`, name-addressed tree/blob/commits/commit/compare) answered from a
//! per-manifest-version ref index with `stale-while-revalidate` + `ETag`, and
//! sha-addressed immutable ones (`tree/<sha>`, `blob/<sha>`,
//! `commits?ref=<sha>`, `commit/<sha>`, `compare/<sha>...<sha>`) rendered once
//! and cached in memory and in the object store.

pub(crate) mod archive;
pub(crate) mod commit;
mod git;
pub(crate) mod handlers;
mod view;
pub(crate) mod write;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post, put},
};
use serde::Serialize;
use utoipa::{IntoParams, ToSchema};

use crate::AppState;

use commit::{create_commit, merge};
use handlers::{
    blob, commit_detail, commits, compare, ref_list, refs, resolve, resolve_root, tree,
};
pub use view::Repo;
pub(crate) use view::{Need, etag_for, json_swr, run};
use write::{create_annotated_tag, delete_branch, delete_tag, put_branch, put_lightweight_tag};

#[derive(Serialize, Clone, ToSchema)]
pub(crate) struct RefInfo {
    pub(crate) name: String,
    pub(crate) sha: String,
}

#[derive(Serialize, ToSchema)]
struct Refs {
    head: Option<RefInfo>,
}

#[derive(Serialize, ToSchema)]
struct RefPage {
    refs: Vec<RefInfo>,
    more: bool,
}

#[derive(Serialize, Clone, ToSchema)]
struct Resolved {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    kind: &'static str,
}

#[derive(Serialize, Clone, ToSchema)]
struct Commit {
    sha: String,
    parents: Vec<String>,
    author: String,
    author_email: String,
    author_date: String,
    committer: String,
    commit_date: String,
    subject: String,
    /// The message body WITHOUT the trailer block (see `trailers`).
    body: String,
    /// Git trailers of the message (`Key: value` lines of the last paragraph,
    /// `git interpret-trailers --parse` rules), in order.
    trailers: Vec<super::trailers::Trailer>,
}

impl Commit {
    fn with_body(mut self, raw: &str) -> Self {
        let (body, trailers) = super::trailers::split_trailers(raw.trim());
        self.body = body;
        self.trailers = trailers;
        self
    }
}

#[derive(Serialize, ToSchema)]
struct Tree {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    entries: Vec<TreeEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<Commit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readme: Option<Readme>,
}

#[derive(Serialize, ToSchema)]
struct TreeEntry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    mode: String,
    size: i64,
    sha: String,
}

#[derive(Serialize, ToSchema)]
struct Blob {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    path: String,
    name: String,
    size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    contents: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    too_large: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct Readme {
    name: String,
    contents: String,
}

#[derive(Serialize, ToSchema)]
struct Commits {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    commits: Vec<Commit>,
    more: bool,
}

#[derive(Serialize, ToSchema)]
struct Stat {
    path: String,
    additions: i64,
    deletions: i64,
}

#[derive(Serialize, ToSchema)]
struct CommitDetail {
    commit: Commit,
    stats: Vec<Stat>,
    patch: String,
}

#[derive(Serialize, ToSchema)]
struct CompareRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Serialize, ToSchema)]
struct CompareFile {
    path: String,
    status: &'static str,
    additions: i64,
    deletions: i64,
}

#[derive(Serialize, ToSchema)]
#[schema(as = CompareResponse)]
struct Compare {
    base: CompareRef,
    head: CompareRef,
    merge_base: String,
    ahead_by: usize,
    behind_by: usize,
    commits: Vec<Commit>,
    files: Vec<CompareFile>,
    patch: String,
    truncated: bool,
}

#[derive(serde::Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CommitQuery {
    #[serde(rename = "ref")]
    ref_: Option<String>,
    path: Option<String>,
    skip: Option<usize>,
    n: Option<usize>,
}

#[derive(serde::Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BlobQuery {
    raw: Option<String>,
}

#[derive(serde::Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RefListQuery {
    prefix: Option<String>,
    q: Option<String>,
    after: Option<String>,
    n: Option<usize>,
}

pub fn router(state: Arc<AppState>) -> Router {
    // D26/D27: repo-scoped endpoints live under the repository's own prefix,
    // `/{owner}/{repo}/api/…` (direct lane) and `/{owner}/{repo}/api-browser/…`
    // (browser lane); the same handlers serve both lanes.
    let mut router = Router::new();
    for base in REPO_API_BASES {
        router = router
            .route(&format!("{base}/refs"), get(refs))
            .route(&format!("{base}/refs/{{kind}}"), get(ref_list))
            .route(
                &format!("{base}/refs/heads/{{*name}}"),
                put(put_branch).delete(delete_branch),
            )
            .route(
                &format!("{base}/refs/tags/{{*name}}"),
                put(put_lightweight_tag).delete(delete_tag),
            )
            .route(&format!("{base}/tags"), post(create_annotated_tag))
            .route(
                &format!("{base}/archive/{{*archive_ref}}"),
                get(archive::archive).head(archive::archive),
            )
            .route(&format!("{base}/resolve"), get(resolve_root))
            .route(&format!("{base}/resolve/"), get(resolve_root))
            .route(&format!("{base}/resolve/{{*rest}}"), get(resolve))
            .route(&format!("{base}/tree/{{*rest}}"), get(tree))
            .route(&format!("{base}/blob/{{*rest}}"), get(blob))
            .route(
                &format!("{base}/commits"),
                get(commits)
                    .post(create_commit)
                    .layer(DefaultBodyLimit::max(commit::request_body_limit(
                        &state.cfg,
                    ))),
            )
            .route(&format!("{base}/merges"), post(merge))
            .route(&format!("{base}/compare/{{*rest}}"), get(compare))
            .route(&format!("{base}/commit/{{sha}}"), get(commit_detail));
    }
    router.with_state(state)
}

/// Route prefixes of the repo-scoped JSON API (D27): one per lane, both
/// *after* the repository prefix. No lane-first forms, no aliases (banner).
pub const REPO_API_BASES: [&str; 2] = ["/{owner}/{repo}/api", "/{owner}/{repo}/api-browser"];
