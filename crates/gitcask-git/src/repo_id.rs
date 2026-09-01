use std::path::{Path, PathBuf};

use crate::GitError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId {
    owner: String,
    name: String,
}

impl RepoId {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Result<Self, GitError> {
        let owner = owner.into();
        let name = name.into();
        validate_part(&owner, "owner")?;
        validate_part(&name, "name")?;
        Ok(RepoId { owner, name })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `repos/<owner>/<repo>/` (gitcask_proto::keys::repo_prefix).
    pub fn store_prefix(&self) -> String {
        gitcask_proto::keys::repo_prefix(&self.owner, &self.name)
    }

    /// `<root>/<owner>/<name>.git` — the on-disk bare repo path.
    pub fn local_dir(&self, root: &Path) -> PathBuf {
        root.join(&self.owner).join(format!("{}.git", self.name))
    }
}

fn validate_part(s: &str, what: &str) -> Result<(), GitError> {
    if s.is_empty() || s.len() > 100 {
        return Err(GitError::InvalidInput(format!(
            "{what} must be 1..=100 chars"
        )));
    }
    if s == ".." {
        return Err(GitError::InvalidInput(format!("{what} may not be '..'")));
    }
    if s.starts_with('.') {
        return Err(GitError::InvalidInput(format!(
            "{what} may not start with '.'"
        )));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(GitError::InvalidInput(format!(
            "{what} must be ASCII [A-Za-z0-9._-]"
        )));
    }
    Ok(())
}

impl std::str::FromStr for RepoId {
    type Err = GitError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let s = s.strip_suffix(".git").unwrap_or(s);
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| GitError::InvalidInput("RepoId must be 'owner/name'".into()))?;
        if owner.is_empty() || name.is_empty() {
            return Err(GitError::InvalidInput(
                "RepoId parts must be non-empty".into(),
            ));
        }
        RepoId::new(owner, name)
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }
    pub fn kind(&self) -> gix_hash::Kind {
        match self {
            ObjectFormat::Sha1 => gix_hash::Kind::Sha1,
            ObjectFormat::Sha256 => gix_hash::Kind::Sha256,
        }
    }
}

impl From<gitcask_config::ObjectFormat> for ObjectFormat {
    fn from(f: gitcask_config::ObjectFormat) -> Self {
        match f {
            gitcask_config::ObjectFormat::Sha1 => ObjectFormat::Sha1,
            gitcask_config::ObjectFormat::Sha256 => ObjectFormat::Sha256,
        }
    }
}

impl From<gix_hash::Kind> for ObjectFormat {
    fn from(k: gix_hash::Kind) -> Self {
        match k {
            gix_hash::Kind::Sha1 => ObjectFormat::Sha1,
            gix_hash::Kind::Sha256 => ObjectFormat::Sha256,
            _ => ObjectFormat::Sha1,
        }
    }
}
