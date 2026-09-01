//! Admin endpoints: `PUT /{owner}/{repo}` (create) and `DELETE /{owner}/{repo}`
//! (delete manifest + prefix objects).

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use gitcask_git::ObjectFormat;

use crate::AppState;
use crate::error::ApiError;
use crate::repo::RepoRoute;

/// `PUT /{owner}/{repo}` — create repo. 201 on new, 409 if it exists.
#[utoipa::path(
    put,
    path = "/{owner}/{repo}",
    tag = "repositories",
    summary = "Create a repository",
    description = "Creates an empty repository using the configured object format, or the `object_format` query override.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("object_format" = Option<String>, Query, description = "Git object format: `sha1` or `sha256`")
    ),
    responses(
        (status = 201, description = "Repository created", body = String, content_type = "text/plain"),
        (status = 400, description = "Invalid repository or object format"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Forwarded mode principal has no write grant"),
        (status = 409, description = "Repository already exists"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub async fn create(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let _principal = st
        .auth
        .require_write(&headers, route.id.owner(), route.id.name())
        .await?;
    let format = match query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .find_map(|part| part.strip_prefix("object_format="))
    {
        Some("sha256") => ObjectFormat::Sha256,
        Some("sha1") => ObjectFormat::Sha1,
        Some(other) => {
            return Err(ApiError::BadRequest(format!(
                "unsupported object format: {other}"
            )));
        }
        None => ObjectFormat::from(st.cfg.git.object_format),
    };
    match st.registry.create(&route.id, format).await {
        Ok(_h) => Ok((StatusCode::CREATED, "created").into_response()),
        Err(gitcask_wal::WalError::AlreadyExists) => {
            Ok((StatusCode::CONFLICT, "already exists").into_response())
        }
        Err(e) => Err(e.into()),
    }
}

/// `DELETE /{owner}/{repo}` — admin-only deletion of the manifest and every object under the repo prefix.
#[utoipa::path(
    delete,
    path = "/{owner}/{repo}",
    tag = "repositories",
    summary = "Delete a repository",
    description = "Permanently deletes the repository manifest and every object below its bucket prefix.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name")
    ),
    responses(
        (status = 204, description = "Repository deleted"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Forwarded mode principal has no admin grant"),
        (status = 404, description = "Repository not found"),
        (status = 503, description = "Object store temporarily unavailable")
    ),
    security(("jwt_bearer" = []))
)]
pub async fn delete(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _principal = st
        .auth
        .require_admin(&headers, route.id.owner(), route.id.name())
        .await?;
    st.registry
        .delete(&route.id)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::NO_CONTENT, "").into_response())
}
