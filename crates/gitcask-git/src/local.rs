use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::error::ge;
use crate::refs::Inner;
use crate::{GitError, ObjectFormat, RepoId};

#[derive(Clone)]
pub struct LocalRepo {
    pub(crate) inner: Arc<Inner>,
}

impl LocalRepo {
    /// Create a bare repo at `<root>/<owner>/<name>.git`.
    pub fn init(root: &Path, id: &RepoId, format: ObjectFormat) -> Result<Self, GitError> {
        let path = id.local_dir(root);
        std::fs::create_dir_all(path.parent().unwrap_or(root)).map_err(|e| GitError::Io(e))?;
        // `git init --bare [--object-format=...] <path>`.
        let mut cmd = std::process::Command::new("git");
        cmd.arg("init").arg("--bare");
        if format == ObjectFormat::Sha256 {
            cmd.arg("--object-format=sha256");
        }
        cmd.arg(&path);
        let out = cmd.output().map_err(GitError::Io)?;
        if !out.status.success() {
            return Err(GitError::Subprocess {
                cmd: "git init".into(),
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        // Deterministic HEAD -> refs/heads/main regardless of the host's
        // init.defaultBranch (git init writes master/main depending on config).
        std::fs::write(path.join("HEAD"), "ref: refs/heads/main\n").map_err(GitError::Io)?;
        // Persist the four fixed settings in one write instead of waiting for
        // four `git config` children per cold repository. `git init` created
        // this private cache config and the per-repo opening gate excludes a
        // concurrent initializer. `pack.writeReverseIndex` makes every local
        // pack writer produce the `.rev` needed to avoid rebuilding it on fetch.
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(path.join("config"))
            .map_err(GitError::Io)?;
        config
            .write_all(
                b"[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n\tallowSidebandAll = true\n[pack]\n\twriteReverseIndex = true\n",
            )
            .map_err(GitError::Io)?;
        let tsr = gix::ThreadSafeRepository::open(&path).map_err(ge)?;
        Ok(LocalRepo {
            inner: Arc::new(Inner {
                id: id.clone(),
                path,
                format,
                tsr: parking_lot::Mutex::new(tsr),
                ingest_lock: tokio::sync::Mutex::new(()),
                refs_cache: parking_lot::Mutex::new(None),
                refs_gen: std::sync::atomic::AtomicU64::new(0),
                refs_parses: std::sync::atomic::AtomicU64::new(0),
            }),
        })
    }

    /// Open an existing bare repo. Returns `Ok(None)` if it does not exist.
    pub fn open(root: &Path, id: &RepoId) -> Result<Option<Self>, GitError> {
        let path = id.local_dir(root);
        if !path.is_dir() || !path.join("HEAD").exists() {
            return Ok(None);
        }
        let tsr = gix::ThreadSafeRepository::open(&path).map_err(ge)?;
        // Detect object format from config.
        let repo = gix::Repository::from(&tsr);
        let kind = repo.object_hash();
        let format = ObjectFormat::from(kind);
        Ok(Some(LocalRepo {
            inner: Arc::new(Inner {
                id: id.clone(),
                path,
                format,
                tsr: parking_lot::Mutex::new(tsr),
                ingest_lock: tokio::sync::Mutex::new(()),
                refs_cache: parking_lot::Mutex::new(None),
                refs_gen: std::sync::atomic::AtomicU64::new(0),
                refs_parses: std::sync::atomic::AtomicU64::new(0),
            }),
        }))
    }

    pub fn id(&self) -> &RepoId {
        &self.inner.id
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn object_format(&self) -> ObjectFormat {
        self.inner.format
    }

    /// Per-call gix handle cloned from the shared thread-safe repository.
    pub fn gix(&self) -> gix::Repository {
        let tsr = self.inner.tsr.lock();
        gix::Repository::from(&*tsr)
    }

    /// Re-open so gix sees new packed-refs / HEAD without remapping pack indexes.
    /// Ref-only writers use this; pack installs use [`refresh`].
    pub fn refresh_refs(&self) -> Result<(), GitError> {
        self.refs_changed();
        let tsr = gix::ThreadSafeRepository::open(&self.inner.path).map_err(ge)?;
        *self.inner.tsr.lock() = tsr;
        Ok(())
    }

    /// Re-open the underlying repository so the odb/refs reflect on-disk
    /// changes from pack/ref writes.
    pub fn refresh(&self) -> Result<(), GitError> {
        self.refs_changed();
        let tsr = gix::ThreadSafeRepository::open(&self.inner.path).map_err(ge)?;
        // Load every pack index / the midx NOW, on this (blocking) thread.
        // gix's odb is lazy: without this the first object lookup after a
        // refresh — a request on an async worker — pays for mmapping and
        // reading a 2.5 GB midx + 2.1 GB idx (prod 2026-08-21: the front
        // stalled 20–30 min while a large repository's pack landed).
        {
            let repo = gix::Repository::from(&tsr);
            let t = std::time::Instant::now();
            // `iter()` snapshots the store with all indices loaded.
            let _ = repo.objects.iter();
            let ms = t.elapsed().as_millis() as u64;
            if ms > 200 {
                tracing::info!(repo = %self.inner.path.display(), ms, "odb indices loaded");
            }
        }
        *self.inner.tsr.lock() = tsr;
        Ok(())
    }

    /// [`refresh`] off the async runtime: re-opening a repository with a
    /// multi-GB index / midx is filesystem work that must never run on a
    /// tokio worker (prod: every other request on the instance stalled for
    /// minutes while a large pack was installed).
    pub async fn refresh_async(&self) -> Result<(), GitError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.refresh())
            .await
            .map_err(|e| GitError::Protocol(format!("refresh task: {e}")))?
    }
}
