use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("gix error: {0}")]
    Gix(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("ref conflict on {name}: expected {expected}, actual {actual}")]
    RefConflict {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("missing object {oid}")]
    MissingObject { oid: String },
    #[error("subprocess `{cmd}` exited {status:?}: {stderr}")]
    Subprocess {
        cmd: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

pub(crate) fn ge<E: std::error::Error + Send + Sync + 'static>(e: E) -> GitError {
    GitError::Gix(Box::new(e))
}

/// Reject ref names that would inject `git update-ref --stdin` commands or
/// poison packed-refs (newlines, NULs, git-illegal bytes).
pub fn validate_ref_name(name: &str) -> Result<(), GitError> {
    if name == "HEAD" {
        return Ok(());
    }
    let bad = name.is_empty()
        || !name.starts_with("refs/")
        || name.bytes().any(|b| {
            matches!(
                b,
                0 | b'\n' | b'\r' | b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'
            )
        })
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock");
    if bad {
        Err(GitError::InvalidInput(format!("invalid ref name {name:?}")))
    } else {
        Ok(())
    }
}

/// Full hex oid (sha1 40 or sha256 64). Empty / all-zeros is a delete/create marker.
pub fn validate_oid(oid: &str) -> Result<(), GitError> {
    if oid.is_empty() || oid.bytes().all(|b| b == b'0') {
        return Ok(());
    }
    let n = oid.len();
    if (n == 40 || n == 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(GitError::InvalidInput(format!("invalid oid {oid:?}")))
    }
}

pub fn validate_ref_update(u: &gitcask_proto::v1::RefUpdate) -> Result<(), GitError> {
    if !u.new_symbolic_target.is_empty() {
        if u.name != "HEAD" {
            return Err(GitError::InvalidInput(format!(
                "symbolic update is only allowed for HEAD, got {:?}",
                u.name
            )));
        }
        return validate_ref_name(&u.new_symbolic_target);
    }
    validate_ref_name(&u.name)?;
    validate_oid(&u.old_oid)?;
    validate_oid(&u.new_oid)
}

#[cfg(test)]
mod validate_ref_tests {
    use super::*;

    #[test]
    fn ref_names_reject_injection() {
        assert!(validate_ref_name("HEAD").is_ok());
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/heads/foo\nupdate refs/heads/main").is_err());
        assert!(validate_ref_name("refs/heads/foo\0bar").is_err());
        assert!(validate_ref_name("../etc/passwd").is_err());
        assert!(validate_oid(&"0".repeat(40)).is_ok());
        assert!(validate_oid("gg").is_err());
        assert!(validate_oid(&"a".repeat(40)).is_ok());
    }
}
