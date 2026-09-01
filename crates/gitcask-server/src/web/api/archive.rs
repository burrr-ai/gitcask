//! Immutable repository archives generated from the local pack set. Only the
//! bounded `(commit, format)` variants are cached in the bucket; a caller-
//! supplied prefix is generated into an unlinked temporary file and streamed.

use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    response::Response,
};
use gitcask_store::{ObjectStore, PutBody, PutMode, PutOptions, StoreError};
use serde::Deserialize;
use sha1::Digest;
use utoipa::IntoParams;

use crate::{AppState, error::ApiError};

use super::write::resolve_commit_target;

#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ArchiveQuery {
    /// Archive format: `tar.gz` (default) or `zip`.
    format: Option<String>,
    /// Prefix prepended to every archive entry. Prefixed variants are never
    /// read from or written to the shared bucket cache.
    prefix: Option<String>,
}

#[derive(Clone, Copy)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

impl ArchiveFormat {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.unwrap_or("tar.gz") {
            "tar.gz" => Ok(Self::TarGz),
            "zip" => Ok(Self::Zip),
            other => Err(ApiError::BadRequest(format!(
                "unsupported archive format {other:?}; expected tar.gz or zip"
            ))),
        }
    }

    fn git_name(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::TarGz => "application/gzip",
            Self::Zip => "application/zip",
        }
    }
}

#[utoipa::path(
    get,
    path = "/{owner}/{repo}/api/archive/{archive_ref}",
    tag = "browsing",
    summary = "Download an immutable repository archive",
    description = "Resolves the revision to a commit, materializes the full pack set, and streams a tar.gz or zip archive. Supports strong ETag validation, HEAD, and single byte ranges. Only requests without `prefix` use the shared bucket cache; prefixed variants are generated per request.",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo" = String, Path, description = "Repository name"),
        ("archive_ref" = String, Path, description = "Commit oid or revision name, including slash-separated branch or tag names"),
        ArchiveQuery
    ),
    responses(
        (status = 200, description = "Complete archive stream"),
        (status = 206, description = "Requested byte range"),
        (status = 304, description = "ETag matched"),
        (status = 400, description = "Invalid format, prefix, or non-commit revision"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Read access denied"),
        (status = 404, description = "Repository or revision not found"),
        (status = 416, description = "Byte range is not satisfiable"),
        (status = 503, description = "Object store temporarily unavailable")
    )
)]
pub(crate) async fn archive(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, archive_ref)): Path<(String, String, String)>,
    Query(query): Query<ArchiveQuery>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let format = ArchiveFormat::parse(query.format.as_deref())?;
    validate_prefix(query.prefix.as_deref())?;
    let method = request.method().clone();
    let headers = request.headers().clone();
    let peer = crate::request_peer(&request);
    drop(request);
    let handle = super::view::open(&state, &headers, &owner, &repo).await?;
    let guard = handle.sync_full().await?;
    let commit = resolve_commit_target(handle.local(), &archive_ref).await?;
    let filename = format!("{repo}.{}", format.git_name());
    let options = || crate::static_object::ServeOptions {
        content_type: format.content_type(),
        filename: Some(&filename),
        accel: state.cfg.server.accel_redirect,
        peer,
        ..Default::default()
    };

    if let Some(prefix) = query.prefix.as_deref() {
        let temporary = generate_stream_archive(&handle, &commit, format, prefix).await?;
        let size = temporary
            .as_file()
            .metadata()
            .map_err(|error| ApiError::Internal(format!("archive metadata: {error}")))?
            .len();
        let etag = archive_etag(&commit, format, prefix);
        let file = temporary.into_file();
        drop(guard);
        return crate::static_object::serve_file(file, size, &etag, &method, &headers, options())
            .await;
    }

    let cache_key = archive_key(&commit, format);
    let store_key = format!(
        "{}{cache_key}.{}",
        gitcask_proto::keys::ARCHIVE_CACHE_DIR,
        format.git_name()
    );
    // Let the conditional/range read be the cache lookup: a hit costs no
    // metadata probe and can be offloaded by the edge. A 404 is the free miss
    // signal; only that path materializes Git objects and writes the cache.
    match crate::static_object::serve(handle.store(), &store_key, &method, &headers, options())
        .await
    {
        Ok(response) => {
            drop(guard);
            return Ok(response);
        }
        Err(ApiError::NotFound(_)) => {}
        Err(error) => return Err(error),
    }

    ensure_cached_archive(&handle, &store_key, &commit, format).await?;
    drop(guard);
    crate::static_object::serve(handle.store(), &store_key, &method, &headers, options()).await
}

fn validate_prefix(prefix: Option<&str>) -> Result<(), ApiError> {
    if prefix.is_some_and(|prefix| prefix.as_bytes().contains(&0)) {
        return Err(ApiError::BadRequest("archive prefix contains NUL".into()));
    }
    Ok(())
}

/// Bucket cache key: deliberately excludes the free-form query prefix.
fn archive_key(commit: &str, format: ArchiveFormat) -> String {
    archive_digest(commit, format, "")
}

/// Strong validator for a request-local prefixed representation.
fn archive_etag(commit: &str, format: ArchiveFormat, prefix: &str) -> String {
    archive_digest(commit, format, prefix)
}

fn archive_digest(commit: &str, format: ArchiveFormat, prefix: &str) -> String {
    let mut digest = sha1::Sha1::new();
    digest.update(b"gitcask-archive-v1\0");
    digest.update(commit.as_bytes());
    digest.update(b"\0");
    digest.update(format.git_name().as_bytes());
    digest.update(b"\0");
    digest.update(prefix.as_bytes());
    hex::encode(digest.finalize())
}

async fn ensure_cached_archive(
    handle: &Arc<gitcask_wal::RepoHandle>,
    store_key: &str,
    commit: &str,
    format: ArchiveFormat,
) -> Result<(), ApiError> {
    loop {
        match handle.begin_task("archive", task_params(commit, format, None)) {
            gitcask_wal::Begin::AlreadyRunning(task) => {
                while !task.wait_done(std::time::Duration::from_mins(1)).await {}
                if handle.store().head(store_key).await?.is_some() {
                    return Ok(());
                }
            }
            gitcask_wal::Begin::Started(task) => {
                task.notice(format!(
                    "generating {} archive for {commit}",
                    format.git_name()
                ));
                let result = async {
                    let temporary =
                        build_archive(handle.local().clone(), commit.to_string(), format, None)
                            .await?;
                    upload_archive(handle, temporary.path(), store_key).await
                }
                .await;
                match result {
                    Ok(()) => {
                        task.finish_ok(format!("{} archive ready", format.git_name()), None);
                        return Ok(());
                    }
                    Err(error) => {
                        let message = error.message();
                        task.finish_err(error.status().as_u16(), message);
                        return Err(error);
                    }
                }
            }
        }
    }
}

async fn generate_stream_archive(
    handle: &Arc<gitcask_wal::RepoHandle>,
    commit: &str,
    format: ArchiveFormat,
    prefix: &str,
) -> Result<tempfile::NamedTempFile, ApiError> {
    loop {
        match handle.begin_task("archive", task_params(commit, format, Some(prefix))) {
            gitcask_wal::Begin::AlreadyRunning(task) => {
                while !task.wait_done(std::time::Duration::from_mins(1)).await {}
            }
            gitcask_wal::Begin::Started(task) => {
                task.notice(format!(
                    "generating {} archive for {commit}",
                    format.git_name()
                ));
                let result = build_archive(
                    handle.local().clone(),
                    commit.to_string(),
                    format,
                    Some(prefix.to_string()),
                )
                .await;
                match result {
                    Ok(temporary) => {
                        task.finish_ok(format!("{} archive ready", format.git_name()), None);
                        return Ok(temporary);
                    }
                    Err(error) => {
                        let message = error.message();
                        task.finish_err(error.status().as_u16(), message);
                        return Err(error);
                    }
                }
            }
        }
    }
}

fn task_params(
    commit: &str,
    format: ArchiveFormat,
    prefix: Option<&str>,
) -> HashMap<String, String> {
    HashMap::from([
        ("commit".to_string(), commit.to_string()),
        ("format".to_string(), format.git_name().to_string()),
        ("prefix".to_string(), prefix.unwrap_or_default().to_string()),
    ])
}

async fn upload_archive(
    handle: &gitcask_wal::RepoHandle,
    path: &FsPath,
    store_key: &str,
) -> Result<(), ApiError> {
    let options = PutOptions {
        mode: PutMode::Create,
        immutable: true,
        ..Default::default()
    };
    match handle
        .store()
        .put(store_key, PutBody::File(path.to_path_buf()), options)
        .await
    {
        Ok(_) | Err(StoreError::PreconditionFailed { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn build_archive(
    local: gitcask_git::LocalRepo,
    commit: String,
    format: ArchiveFormat,
    prefix: Option<String>,
) -> Result<tempfile::NamedTempFile, ApiError> {
    tokio::task::spawn_blocking(move || {
        let parent = local.path().join("gitcask-archives");
        std::fs::create_dir_all(&parent)
            .map_err(|error| ApiError::Internal(format!("archive cache directory: {error}")))?;
        let temporary = tempfile::Builder::new()
            .prefix(".archive-")
            .tempfile_in(parent)
            .map_err(|error| ApiError::Internal(format!("archive temporary file: {error}")))?;
        let mut command = std::process::Command::new("git");
        command
            .current_dir(local.path())
            .env("GIT_DIR", local.path())
            .arg("archive")
            .arg(format!("--format={}", format.git_name()))
            .arg(format!("--output={}", temporary.path().display()));
        if let Some(prefix) = prefix {
            command.arg(format!("--prefix={prefix}"));
        }
        let result = command.arg(&commit).output().map_err(|error| {
            ApiError::Internal(format!("running git archive for {commit}: {error}"))
        })?;
        if !result.status.success() {
            return Err(ApiError::Internal(format!(
                "git archive exited {:?}: {}",
                result.status.code(),
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        Ok(temporary)
    })
    .await
    .map_err(|error| ApiError::Internal(format!("archive task: {error}")))?
}
