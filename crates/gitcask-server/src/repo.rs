//! Repository path normalization and extraction.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path};
use axum::http::{Request, Uri, request::Parts};
use gitcask_git::RepoId;

use crate::error::ApiError;

/// Marks requests whose repository segment originally ended in `.git`.
#[derive(Clone, Copy, Debug)]
struct HadGitSuffix;

/// A validated repository route.
#[derive(Debug)]
pub struct RepoRoute {
    pub id: RepoId,
    /// True when the request used the `.git` suffix.
    pub had_git_suffix: bool,
}

impl<S> FromRequestParts<S> for RepoRoute
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(params) = Path::<HashMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::NotFound("repository".into()))?;
        let owner = params
            .get("owner")
            .ok_or_else(|| ApiError::NotFound("repository".into()))?;
        let repo = params
            .get("repo")
            .ok_or_else(|| ApiError::NotFound("repository".into()))?;
        let id = RepoId::new(owner, repo).map_err(|e| ApiError::NotFound(e.to_string()))?;
        Ok(Self {
            id,
            had_git_suffix: parts.extensions.get::<HadGitSuffix>().is_some(),
        })
    }
}

/// Strip `.git` from the second path segment before the repository router sees
/// the request. A later `.git` (for example in a blob path) is untouched.
pub async fn normalize_git_suffix(mut req: Request<Body>) -> Request<Body> {
    let Some(path) = normalized_path(req.uri().path()) else {
        return req;
    };
    let path_and_query = match req.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let Ok(path_and_query) = path_and_query.parse() else {
        return req;
    };
    let mut parts = req.uri().clone().into_parts();
    parts.path_and_query = Some(path_and_query);
    let Ok(uri) = Uri::from_parts(parts) else {
        return req;
    };
    *req.uri_mut() = uri;
    req.extensions_mut().insert(HadGitSuffix);
    req
}

fn normalized_path(path: &str) -> Option<String> {
    let path = path.strip_prefix('/')?;
    let mut segments = path.splitn(3, '/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git")?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let mut normalized = format!("/{owner}/{repo}");
    if let Some(tail) = segments.next() {
        normalized.push('/');
        normalized.push_str(tail);
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn normalizes_only_the_repository_segment() -> anyhow::Result<()> {
        let request = Request::builder()
            .uri("/acme/project.git/api/blob/main/path.git?raw=1")
            .body(Body::empty())?;
        let request = normalize_git_suffix(request).await;
        assert_eq!(request.uri(), "/acme/project/api/blob/main/path.git?raw=1");
        assert!(request.extensions().get::<HadGitSuffix>().is_some());

        let request = Request::builder()
            .uri("/acme/project/api/blob/main/path.git")
            .body(Body::empty())?;
        let request = normalize_git_suffix(request).await;
        assert_eq!(request.uri(), "/acme/project/api/blob/main/path.git");
        assert!(request.extensions().get::<HadGitSuffix>().is_none());
        Ok(())
    }
}
