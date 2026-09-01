//! Request-lifetime repository views and rendered-response caching.

use std::future::Future;
use std::sync::Arc;

use axum::{http::HeaderMap, response::Response};
use gitcask_proto::keys;
use gitcask_store::{GetOptions, ObjectStore, Prefixed, PutBody, PutMode};
use gitcask_wal::RepoHandle;
use serde::Serialize;

use crate::sse::Rendered;
use crate::{AppState, cache::RefIndex, error::ApiError};

const IMMUTABLE: &str = "private, max-age=31536000, immutable";
const SWR: &str = "private, max-age=0, stale-while-revalidate=60";
/// One synced view of a repository for the duration of a request.
pub struct Repo {
    pub(super) id: String,
    pub(super) local: gitcask_git::LocalRepo,
    pub(crate) index: Arc<RefIndex>,
    handle: Arc<RepoHandle>,
    /// Whether objects are readable ([`Need::Objects`] satisfied).
    pub(super) objects: bool,
    /// Shared render cache (object store).
    shared: Option<Prefixed>,
}

impl Repo {
    /// Upgrade a refs-level view to objects (used by `resolve` for raw revisions).
    pub(super) async fn need_objects(&mut self, state: &AppState) -> Result<(), ApiError> {
        if self.objects {
            return Ok(());
        }
        let guard = self.handle.sync_full().await?;
        drop(guard);
        self.objects = true;
        self.shared = shared_cache(state, &self.handle);
        Ok(())
    }
}

/// What a request needs from the local copy: refs only (cheap, always
/// possible) or objects too (all packs on disk).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    Refs,
    Objects,
}

fn shared_cache(state: &AppState, handle: &RepoHandle) -> Option<Prefixed> {
    state
        .cfg
        .cache
        .shared_render_cache
        .then(|| handle.store().clone())
}

pub(super) async fn open(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<Arc<RepoHandle>, ApiError> {
    state.auth.require_read(headers, owner, name).await?;
    let id = gitcask_git::RepoId::new(owner, name)
        .map_err(|_| ApiError::NotFound("repository".into()))?;
    Ok(state.registry.open(&id).await?)
}

pub(super) async fn view(
    state: &AppState,
    handle: Arc<RepoHandle>,
    need: Need,
) -> Result<Repo, ApiError> {
    let (guard, objects) = match need {
        Need::Refs => (handle.sync_refs().await?, false),
        Need::Objects => {
            let guard = handle.sync_full().await?;
            (guard, true)
        }
    };
    // The guard is held until the local handle has been cloned and the ref
    // index for this manifest version exists. The local repository itself is
    // thread-safe and subsequent git commands read its synced state.
    let local = handle.local().clone();
    let version = handle
        .manifest_version()
        .map(|version| version.as_str().to_string())
        .unwrap_or_default();
    let id = handle.id().to_string();
    let index = state
        .caches
        .ref_index
        .get_or_build(&id, &version, || local.refs())
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    drop(guard);
    let shared = shared_cache(state, &handle);
    Ok(Repo {
        id,
        local,
        index,
        handle,
        objects,
        shared,
    })
}

fn shared_key(cache_key: &str) -> String {
    use sha1::Digest;
    let hash = sha1::Sha1::digest(cache_key.as_bytes());
    format!("{}{}.json", keys::API_CACHE_DIR, hex::encode(hash))
}

/// Run one endpoint: auth + open, immutable caches, then either a plain
/// response or (when the answer needs long work and the client accepts it)
/// the SSE envelope streaming the repo's progress until the result.
///
/// `immutable_key` is the complete cache contract: this function checks both
/// cache tiers before materializing the repository. The endpoint work must not
/// repeat the lookup; it only renders a miss and calls [`finish`] to populate it.
pub(crate) async fn run<F, Fut>(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    need: Need,
    immutable_key: Option<String>,
    work: F,
) -> Result<Response, ApiError>
where
    F: FnOnce(Repo) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Rendered, ApiError>> + Send + 'static,
{
    let handle = open(state, headers, owner, name).await?;
    let slow = need == Need::Objects && !handle.packs_ready();
    if let Some(key) = &immutable_key {
        if let Some(hit) = state.caches.api_immutable.get(key) {
            metrics::counter!("gitcask_api_immutable_hit", "tier" => "memory").increment(1);
            return Ok(Rendered::json(hit, IMMUTABLE, None).into_response(headers));
        }
        if slow && state.cfg.cache.shared_render_cache {
            if let Ok(gitcask_store::GetResult::Object { body, meta }) = handle
                .store()
                .get(&shared_key(key), GetOptions::default())
                .await
            {
                if let Ok(bytes) = gitcask_store::util::collect(body, meta.size as usize).await {
                    metrics::counter!("gitcask_api_immutable_hit", "tier" => "store").increment(1);
                    state
                        .caches
                        .api_immutable
                        .insert(key.clone(), bytes.clone());
                    return Ok(Rendered::json(bytes, IMMUTABLE, None).into_response(headers));
                }
            }
        }
    }
    if slow && crate::sse::wants_sse(headers) {
        let sources = vec![handle.subscribe_progress()];
        let state = state.clone();
        let future = async move {
            let repo = view(&state, handle, need).await?;
            work(repo).await
        };
        return Ok(crate::sse::envelope(sources, future));
    }
    let repo = view(state, handle, need).await?;
    Ok(work(repo).await?.into_response(headers))
}

pub(crate) fn etag_for(sha: &str) -> String {
    format!("\"{sha}\"")
}

pub(crate) fn json_swr<T: Serialize>(value: &T, etag: Option<&str>) -> Rendered {
    Rendered::json(json_bytes(value), SWR, etag.map(str::to_string))
}

pub(super) fn json_bytes<T: Serialize>(value: &T) -> bytes::Bytes {
    bytes::Bytes::from(serde_json::to_vec(value).unwrap_or_default())
}

/// Finish a ref-or-sha addressed request: immutable (+LRU + shared cache) or
/// SWR + ETag (304 handled by `Rendered::into_response`).
pub(super) fn finish(
    state: &AppState,
    repo: &Repo,
    immutable: bool,
    cache_key: &str,
    sha: &str,
    body: bytes::Bytes,
) -> Rendered {
    if immutable {
        state
            .caches
            .api_immutable
            .insert(cache_key.to_string(), body.clone());
        if let Some(store) = &repo.shared {
            let store = store.clone();
            let key = shared_key(cache_key);
            let body = body.clone();
            tokio::spawn(async move {
                if let Err(error) = store
                    .put(&key, PutBody::Bytes(body), PutMode::Overwrite.into())
                    .await
                {
                    tracing::debug!(%error, key, "shared render cache put failed");
                }
            });
        }
        return Rendered::json(body, IMMUTABLE, None);
    }
    Rendered::json(body, SWR, Some(etag_for(sha)))
}

pub(super) const IMMUTABLE_CACHE_CONTROL: &str = IMMUTABLE;
pub(super) const SWR_CACHE_CONTROL: &str = SWR;
