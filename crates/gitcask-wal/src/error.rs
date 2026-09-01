//! Error types for the WAL crate.

use gitcask_store::StoreError;
use thiserror::Error;

/// Coordination-layer error. Re-exported from `gitcask_store::coord`.
pub use gitcask_store::coord::CoordError;

/// WAL-level error.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("repository not found")]
    NotFound,
    #[error("repository already exists")]
    AlreadyExists,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Coord(#[from] CoordError),
    #[error(transparent)]
    Git(#[from] gitcask_git::GitError),
    #[error("publish failed: {msg}")]
    Publish { msg: String, retryable: bool },
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("retry exhausted after {attempts} attempts")]
    Retry { attempts: u32 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl WalError {
    /// Whether retrying the request may succeed after a transient store failure.
    pub fn is_retryable(&self) -> bool {
        match self {
            WalError::Store(error) | WalError::Coord(CoordError::Store(error)) => {
                error.is_retryable()
            }
            WalError::Publish { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

/// Per-ref error within a publish result.
#[derive(Debug, Clone, Error)]
pub enum RefError {
    #[error("non-fast-forward")]
    NonFastForward,
    #[error("conflict: expected {expected}, got {actual}")]
    Conflict { expected: String, actual: String },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("ref missing")]
    Missing,
}

impl From<WalError> for RefError {
    fn from(e: WalError) -> Self {
        RefError::Rejected(e.to_string())
    }
}
