//! Persistent local state for a RepoHandle, stored in the repo dir so restarts
//! skip already-applied log entries.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::WalError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    /// Opaque version string of the last manifest we applied.
    pub manifest_version: Option<String>,
    /// Last applied log entry sequence.
    pub applied_seq: u64,
    /// Manifest revision (diagnostics).
    pub revision: u64,
    /// Manifest revision whose live pack set (`Manifest.packs`) is fully
    /// installed locally. `!= revision` means refs were applied (refs-first
    /// sync) but packs still need reconciling before serving objects.
    #[serde(default)]
    pub packs_revision: u64,
    /// Pack checksums superseded by COMPACT entries that were applied at the
    /// refs level only; removed locally on the next full (packs) sync.
    #[serde(default)]
    pub pending_pack_removals: Vec<String>,
}

impl RepoState {
    /// True when the local pack set matches the applied manifest.
    pub fn packs_ready(&self) -> bool {
        self.packs_revision == self.revision && self.pending_pack_removals.is_empty()
    }
}

impl Default for RepoState {
    fn default() -> Self {
        RepoState {
            manifest_version: None,
            applied_seq: 0,
            revision: 0,
            packs_revision: 0,
            pending_pack_removals: Vec::new(),
        }
    }
}

impl RepoState {}

const STATE_FILE: &str = "gitcask-state.json";

pub fn state_path(repo_dir: &Path) -> std::path::PathBuf {
    repo_dir.join(STATE_FILE)
}

pub fn load_state(repo_dir: &Path) -> RepoState {
    let path = state_path(repo_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => RepoState::default(),
    }
}

pub fn save_state(repo_dir: &Path, state: &RepoState) -> Result<(), WalError> {
    let path = state_path(repo_dir);
    let text = serde_json::to_string_pretty(state).map_err(|e| WalError::Corrupt(e.to_string()))?;
    // Refs sync, pack materialization, and checkpoint publication may persist
    // the same handle concurrently. Each writer needs its own same-directory
    // temp file so one atomic rename cannot consume another writer's input.
    let mut tmp = tempfile::Builder::new()
        .prefix(".gitcask-state-")
        .tempfile_in(repo_dir)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "creating state temp file in {}: {error}",
                    repo_dir.display()
                ),
            )
        })?;
    tmp.write_all(text.as_bytes()).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("writing state temp file in {}: {error}", repo_dir.display()),
        )
    })?;
    tmp.persist(&path).map_err(|error| {
        std::io::Error::new(
            error.error.kind(),
            format!(
                "persisting state temp file to {}: {}",
                path.display(),
                error.error
            ),
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn concurrent_saves_are_atomic() -> Result<(), Box<dyn std::error::Error>> {
        const WRITERS: usize = 32;
        const SAVES_PER_WRITER: usize = 100;

        let dir = tempfile::tempdir()?;
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut writers = Vec::with_capacity(WRITERS);
        for writer in 0..WRITERS {
            let path = dir.path().to_path_buf();
            let barrier = barrier.clone();
            let revision = u64::try_from(writer)?;
            writers.push(std::thread::spawn(move || -> Result<(), WalError> {
                barrier.wait();
                for _ in 0..SAVES_PER_WRITER {
                    save_state(
                        &path,
                        &RepoState {
                            revision,
                            ..RepoState::default()
                        },
                    )?;
                }
                Ok(())
            }));
        }
        for writer in writers {
            writer
                .join()
                .map_err(|_| std::io::Error::other("state writer thread panicked"))??;
        }

        let saved: RepoState =
            serde_json::from_str(&std::fs::read_to_string(state_path(dir.path()))?)?;
        assert!(saved.revision < u64::try_from(WRITERS)?);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
}
