//! Registry: process-wide map of RepoId -> Arc<RepoHandle>.

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use futures::StreamExt;
use gitcask_git::{LocalRepo, ObjectFormat, RepoId};
use gitcask_proto::WAL_FORMAT_VERSION;
use gitcask_proto::keys;
use gitcask_proto::v1::Manifest;
use gitcask_store::coord::get_message;
use gitcask_store::{DynStore, ObjectStore, Prefixed, PutBody, PutMode, StoreError};
use prost::Message;

use crate::error::WalError;
use crate::handle::RepoHandle;
use crate::state::{RepoState, load_state, save_state};

pub struct Registry {
    store: DynStore,
    cfg: Arc<gitcask_config::Config>,
    cache_root: std::path::PathBuf,
    repos: DashMap<RepoId, Arc<RepoHandle>>,
    /// Per-repo single-flight guard for open/create so two concurrent first
    /// requests never both `git init` / materialize the same repo.
    opening: DashMap<RepoId, Arc<tokio::sync::Mutex<()>>>,
    /// Background task log + (repo, kind) locks for this instance.
    tasks: Arc<crate::tasks::Tasks>,
}

#[derive(Default, Debug)]
pub struct EvictReport {
    pub evicted: usize,
    pub remaining_bytes: u64,
}

impl Registry {
    pub fn new(store: DynStore, cfg: Arc<gitcask_config::Config>) -> Arc<Self> {
        let cache_root = cfg.cache.dir.clone();
        Arc::new(Registry {
            store,
            cfg,
            cache_root,
            repos: DashMap::new(),
            opening: DashMap::new(),
            tasks: crate::tasks::Tasks::new(),
        })
    }

    pub fn store(&self) -> &DynStore {
        &self.store
    }

    pub fn tasks(&self) -> &Arc<crate::tasks::Tasks> {
        &self.tasks
    }

    pub fn config(&self) -> &Arc<gitcask_config::Config> {
        &self.cfg
    }

    /// Open an existing repo. Err(NotFound) if manifest.pb absent.
    pub async fn open(&self, id: &RepoId) -> Result<Arc<RepoHandle>, WalError> {
        let open_started = Instant::now();
        // Every request that touches a repo goes through here: tag the
        // enclosing `http.request` span (no-op outside one).
        tracing::Span::current().record("repo", tracing::field::display(id));
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }
        let gate = self.opening.entry(id.clone()).or_default().clone();
        let gate_started = Instant::now();
        let _g = gate.lock().await;
        let opening_wait_ms = u64::try_from(gate_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }

        let prefix = id.store_prefix();
        let prefixed = Prefixed::new(self.store.clone(), prefix);

        // Read manifest (NotFound if absent)
        let store_started = Instant::now();
        let (meta, manifest) = match get_message::<Manifest>(&prefixed, keys::MANIFEST).await? {
            Some(v) => v,
            None => return Err(WalError::NotFound),
        };
        let store_get_ms = u64::try_from(store_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Opening/initializing a local repo does filesystem work and waits for
        // its `git init` child. Keep all of it off the async worker so
        // unrelated cold repositories can keep polling their store GETs even
        // on a two-thread runtime.
        let local_started = Instant::now();
        let cache_root = self.cache_root.clone();
        let open_id = id.clone();
        let format = parse_object_format(&manifest.object_format);
        let (local, state) = tokio::task::spawn_blocking(move || {
            let local = match LocalRepo::open(&cache_root, &open_id)? {
                Some(local) => local,
                None => LocalRepo::init(&cache_root, &open_id, format)?,
            };
            let state = load_state(local.path());
            Ok::<_, WalError>((local, state))
        })
        .await
        .map_err(|error| WalError::Corrupt(format!("cold open task failed: {error}")))??;
        let local_open_ms = u64::try_from(local_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let state_is_behind = state.applied_seq < manifest.head_seq;
        let manifest_version = meta.version.clone();

        let handle = RepoHandle::new(
            id.clone(),
            local,
            prefixed,
            self.cfg.clone(),
            manifest.clone(),
            Some(manifest_version.clone()),
            state,
            self.tasks.clone(),
        );

        let handle = Arc::new(handle);
        handle.set_self_arc(handle.clone());

        // The manifest GET above is already fresh. Apply that exact value
        // directly instead of issuing a second manifest GET merely to learn
        // what we already hold. Cold open remains one manifest round, followed
        // by checkpoint/log objects in parallel/sequence as required.
        let delta_started = Instant::now();
        if state_is_behind {
            crate::sync::apply_delta(&handle, &manifest, &manifest_version).await?;
        }
        let delta_ms = u64::try_from(delta_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        tracing::info!(
            repo = %id,
            opening_wait_ms,
            store_get_ms,
            local_open_ms,
            delta_ms,
            elapsed_ms = u64::try_from(open_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "cold repository open"
        );

        self.repos.insert(id.clone(), handle.clone());
        Ok(handle)
    }

    /// Delete a repository: every object under its store prefix, the cached
    /// handle and the local copy. Err(NotFound) if the manifest does not exist.
    /// Other instances notice on their next freshness check (manifest GET -> 404).
    pub async fn delete(&self, id: &RepoId) -> Result<(), WalError> {
        let prefixed = Prefixed::new(self.store.clone(), id.store_prefix());
        if get_message::<Manifest>(&prefixed, keys::MANIFEST)
            .await?
            .is_none()
        {
            return Err(WalError::NotFound);
        }
        // Pending markers live outside the repository prefix. Remove the
        // marker before the manifest commit point disappears; an absent marker
        // is already success for every store backend.
        self.store
            .delete(&keys::pending_key(id.owner(), id.name()), None)
            .await?;
        // Drop the handle first so no request on this instance publishes into a
        // prefix that is being removed; in-flight requests hold their own Arc.
        self.repos.remove(id);
        // Manifest first: it is the linearization point, so the repo disappears
        // atomically for readers; remaining objects are unreferenced garbage.
        prefixed.delete(keys::MANIFEST, None).await?;
        let mut after: Option<String> = None;
        loop {
            let mut stream = prefixed.list("", after.as_deref());
            let mut last = None;
            while let Some(res) = stream.next().await {
                let m = res?;
                prefixed.delete(&m.key, None).await?;
                last = Some(m.key);
            }
            match last {
                Some(k) => after = Some(k),
                None => break,
            }
        }
        let local_dir = id.local_dir(&self.cache_root);
        if local_dir.exists() {
            tokio::fs::remove_dir_all(&local_dir)
                .await
                .map_err(WalError::Io)?;
        }
        Ok(())
    }

    /// CAS-create manifest.pb (PutMode::Create). Err(AlreadyExists) on 412.
    pub async fn create(
        &self,
        id: &RepoId,
        format: ObjectFormat,
    ) -> Result<Arc<RepoHandle>, WalError> {
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }
        let gate = self.opening.entry(id.clone()).or_default().clone();
        let _g = gate.lock().await;
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }

        let prefix = id.store_prefix();
        let prefixed = Prefixed::new(self.store.clone(), prefix);

        // Create manifest with PutMode::Create
        let manifest = Manifest {
            format_version: WAL_FORMAT_VERSION,
            repo: id.to_string(),
            object_format: format.as_str().to_string(),
            head_seq: 0,
            min_seq: 0,
            checkpoint: None,
            log_segments: vec![],
            packs: vec![],
            updated_at: Some(gitcask_proto::time::now()),
            writer: crate::handle::instance_id(),
            revision: 1,
        };

        let buf = manifest.encode_to_vec();
        match prefixed
            .put(
                keys::MANIFEST,
                PutBody::Bytes(bytes::Bytes::from(buf)),
                PutMode::Create.into(),
            )
            .await
        {
            Ok(meta) => {
                // Creating the local cache has the same blocking git/fs work
                // as a cold open; the manifest PUT remains async above.
                let cache_root = self.cache_root.clone();
                let create_id = id.clone();
                let (local, state) = tokio::task::spawn_blocking(move || {
                    let local = LocalRepo::init(&cache_root, &create_id, format)?;
                    let state = RepoState::default();
                    save_state(local.path(), &state)?;
                    Ok::<_, WalError>((local, state))
                })
                .await
                .map_err(|error| {
                    WalError::Corrupt(format!("repository init task failed: {error}"))
                })??;

                let handle = RepoHandle::new(
                    id.clone(),
                    local,
                    prefixed,
                    self.cfg.clone(),
                    manifest,
                    Some(meta.version),
                    state,
                    self.tasks.clone(),
                );
                let handle = Arc::new(handle);
                handle.set_self_arc(handle.clone());

                self.repos.insert(id.clone(), handle.clone());
                Ok(handle)
            }
            Err(StoreError::PreconditionFailed { .. }) => Err(WalError::AlreadyExists),
            Err(e) => Err(WalError::Store(e)),
        }
    }

    /// Open or create.
    pub async fn open_or_create(
        &self,
        id: &RepoId,
        format: ObjectFormat,
    ) -> Result<Arc<RepoHandle>, WalError> {
        match self.open(id).await {
            Ok(h) => Ok(h),
            Err(WalError::NotFound) => self.create(id, format).await,
            Err(e) => Err(e),
        }
    }

    /// Repositories currently materialized on this instance, sorted by owner and name.
    pub fn cached_repos(&self) -> Vec<RepoId> {
        let mut repos: Vec<_> = self.repos.iter().map(|entry| entry.key().clone()).collect();
        repos.sort_by(|a, b| {
            a.owner()
                .cmp(b.owner())
                .then_with(|| a.name().cmp(b.name()))
        });
        repos
    }

    /// Evict idle repositories and relieve disk pressure past the high watermark.
    pub async fn evict_idle(&self) -> Result<EvictReport, WalError> {
        let evict_after = self.cfg.cache.evict_idle_after;
        let now = std::time::Instant::now();

        let mut evicted = 0;
        let mut candidates: Vec<(RepoId, Instant, Arc<RepoHandle>, std::path::PathBuf)> =
            Vec::new();

        // In-use checks happen again while evicting: a request may acquire a
        // ReadGuard after this snapshot.
        for entry in self.repos.iter() {
            let handle = entry.value();
            candidates.push((
                entry.key().clone(),
                handle.last_access(),
                handle.clone(),
                handle.local.path().to_path_buf(),
            ));
        }

        // Sort by oldest access first.
        candidates.sort_by_key(|(_, t, _, _)| *t);

        // Walking repository directories is blocking filesystem work. Do it
        // once for the pass, then carry each result through eviction instead
        // of walking a repository again before deleting it.
        let paths: Vec<_> = candidates
            .iter()
            .map(|(_, _, _, path)| path.clone())
            .collect();
        let sizes = tokio::task::spawn_blocking(move || {
            paths.iter().map(|path| dir_size(path)).collect::<Vec<_>>()
        })
        .await
        .map_err(|error| WalError::Corrupt(format!("cache size task failed: {error}")))?;
        let mut total_bytes: u64 = sizes.iter().sum();
        // Disk pressure is measured for the whole filesystem, including data
        // outside the cache. When above the high watermark, evict oldest
        // repositories until the filesystem reaches a 10%-lower target.
        let pressure_target = match disk_usage(&self.cfg.cache.dir) {
            Some((used, total)) if total > 0 && self.cfg.cache.disk_high_watermark > 0.0 => {
                let frac = used as f64 / total as f64;
                metrics::gauge!("gitcask_cache_disk_used_fraction").set(frac);
                if frac > self.cfg.cache.disk_high_watermark {
                    let low = ((self.cfg.cache.disk_high_watermark - 0.10).max(0.0) * total as f64)
                        as u64;
                    let over = used.saturating_sub(low);
                    tracing::warn!(
                        used,
                        total,
                        over,
                        "cache disk above high watermark: evicting repositories"
                    );
                    Some(total_bytes.saturating_sub(over))
                } else {
                    None
                }
            }
            _ => None,
        };

        for ((id, last_access, handle, path), bytes) in candidates.iter().zip(sizes) {
            let idle = now.duration_since(*last_access) > evict_after;
            let pressured = pressure_target.is_some_and(|target| total_bytes > target);
            if !idle && !pressured {
                continue;
            }
            // Hold both gates through removal. `sync_mutex` excludes a sync
            // beginning between the idle snapshot and removal; `rw.write`
            // excludes leaked/long-lived request ReadGuards. The old code only
            // probed sync_mutex and immediately dropped it, so it could delete
            // packs underneath an active reader.
            let Ok(_sync) = handle.sync_mutex.try_lock() else {
                continue;
            };
            let Ok(_write) = handle.rw.try_write() else {
                continue;
            };
            self.repos.remove(id);
            self.reclaim_opening_gate(id);
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = std::fs::remove_dir_all(&path) {
                    tracing::debug!(path = %path.display(), %error, "cache directory removal failed");
                }
            })
            .await
            .map_err(|error| WalError::Corrupt(format!("cache removal task failed: {error}")))?;
            total_bytes = total_bytes.saturating_sub(bytes);
            evicted += 1;
        }

        Ok(EvictReport {
            evicted,
            remaining_bytes: total_bytes,
        })
    }

    fn reclaim_opening_gate(&self, id: &RepoId) {
        let _removed = self
            .opening
            .remove_if(id, |_, gate| Arc::strong_count(gate) == 1);
    }
}

fn parse_object_format(s: &str) -> ObjectFormat {
    match s {
        "sha256" => ObjectFormat::Sha256,
        _ => ObjectFormat::Sha1,
    }
}

/// Bytes a repo directory occupies, counting hard-linked files once.
fn dir_size(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fn walk(p: &std::path::Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    total += walk(&path, seen);
                } else if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                    total += meta.len();
                }
            }
        }
        total
    }
    walk(path, &mut std::collections::HashSet::new())
}

/// (used, total) bytes of the filesystem holding `path` (statvfs).
fn disk_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let total = st.f_blocks as u64 * st.f_frsize as u64;
    let avail = st.f_bavail as u64 * st.f_frsize as u64;
    Some((total.saturating_sub(avail), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitcask_store::memory::MemoryStore;

    #[tokio::test]
    async fn evict_reclaims_idle_opening_gates() {
        let cache = tempfile::tempdir().unwrap();
        let mut cfg = gitcask_config::Config::default();
        cfg.cache.dir = cache.path().to_path_buf();
        cfg.cache.evict_idle_after = std::time::Duration::ZERO;
        let registry = Registry::new(MemoryStore::shared(), Arc::new(cfg));

        for index in 0..64 {
            let id = RepoId::new("bounded", format!("repo-{index}")).unwrap();
            registry.create(&id, ObjectFormat::Sha1).await.unwrap();
            assert_eq!(registry.evict_idle().await.unwrap().evicted, 1);
            assert!(registry.opening.is_empty());
        }
    }

    #[test]
    fn opening_gate_in_use_is_not_reclaimed() {
        let registry = Registry::new(
            MemoryStore::shared(),
            Arc::new(gitcask_config::Config::default()),
        );
        let id = RepoId::new("bounded", "opening").unwrap();
        let active = Arc::new(tokio::sync::Mutex::new(()));
        registry.opening.insert(id.clone(), active.clone());

        registry.reclaim_opening_gate(&id);
        assert!(registry.opening.contains_key(&id));

        drop(active);
        registry.reclaim_opening_gate(&id);
        assert!(!registry.opening.contains_key(&id));
    }
}
