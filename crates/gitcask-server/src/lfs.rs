//! Git LFS batch API + basic transfer (download/upload/verify). Objects live at
//! `lfs/objects/<oid[0:2]>/<oid[2:4]>/<oid>` in the repo-scoped store.
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use gitcask_proto::keys;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::ApiError;
use crate::repo::RepoRoute;
use crate::smart::open_repo;
use crate::stream::body_to_async_read;
use gitcask_store::{ObjectStore, ObjectStoreExt, PutBody, PutMode};

const LFS_BATCH_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub operation: String, // "upload" | "download"
    pub transfers: Option<Vec<String>>,
    pub objects: Vec<BatchObject>,
}

#[derive(Debug, Deserialize)]
pub struct BatchObject {
    pub oid: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
struct BatchResponse<'a> {
    transfer: &'a str,
    objects: Vec<BatchRespObject>,
}

#[derive(Debug, Serialize)]
struct BatchRespObject {
    oid: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    authenticated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Actions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<LfsError>,
}

#[derive(Debug, Serialize)]
struct Actions {
    #[serde(skip_serializing_if = "Option::is_none")]
    download: Option<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<Action>,
}

#[derive(Debug, Serialize)]
struct Action {
    href: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LfsError {
    code: u16,
    message: String,
}

#[derive(Deserialize)]
pub(crate) struct ObjectPath {
    oid: String,
}

/// `POST /{repo}/info/lfs/objects/batch`
pub(crate) async fn batch(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let body_bytes = axum::body::to_bytes(body, LFS_BATCH_LIMIT)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let body: BatchRequest = serde_json::from_slice(&body_bytes)
        .map_err(|e| ApiError::BadRequest(format!("invalid lfs batch: {e}")))?;
    if !st.cfg.lfs.enabled {
        return Err(ApiError::NotFound("lfs disabled".into()));
    }
    match body.operation.as_str() {
        "upload" => {
            st.auth
                .require_write(&headers, route.id.owner(), route.id.name())
                .await?;
        }
        "download" => {
            st.auth
                .require_read(&headers, route.id.owner(), route.id.name())
                .await?;
        }
        _ => {
            return Err(ApiError::BadRequest(
                "LFS operation must be upload or download".into(),
            ));
        }
    }
    let handle = open_repo(&st, &route.id, false).await?;
    let store = handle.store().clone();
    let base = base_url(&st, &route, &headers);
    let cfg = st.cfg.clone();

    let is_upload = body.operation == "upload";
    for o in &body.objects {
        require_lfs_oid(&o.oid)?;
    }
    let mut local = Vec::with_capacity(body.objects.len());
    for o in &body.objects {
        let exists = match store.exists(&keys::lfs_key(&o.oid)).await {
            Ok(exists) => exists,
            Err(error) if error.is_retryable() => return Err(error.into()),
            Err(_) => false,
        };
        local.push(exists);
    }

    let mut objs = Vec::with_capacity(body.objects.len());
    for (o, exists) in body.objects.iter().zip(local) {
        let key = keys::lfs_key(&o.oid);
        let mut actions = Actions {
            download: None,
            upload: None,
            verify: None,
        };
        if is_upload && exists {
            // NO `actions` key at all means the server already has the object.
            objs.push(BatchRespObject {
                oid: o.oid.clone(),
                size: o.size,
                authenticated: Some(true),
                actions: None,
                error: None,
            });
            continue;
        }
        if is_upload {
            actions.upload = Some(Action {
                href: format!("{base}/info/lfs/objects/{}", o.oid),
                header: None,
                expires_in: None,
            });
            actions.verify = Some(Action {
                href: format!("{base}/info/lfs/verify"),
                header: None,
                expires_in: None,
            });
        } else if exists {
            let href = match cfg.lfs.serve_via {
                gitcask_config::LfsServe::SignedUrl => store
                    .signed_get_url(&key, st.cfg.lfs.signed_url_ttl)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| format!("{base}/info/lfs/objects/{}", o.oid)),
                _ => format!("{base}/info/lfs/objects/{}", o.oid),
            };
            actions.download = Some(Action {
                href,
                header: None,
                expires_in: None,
            });
        } else {
            // missing object on download: per-object 404 error
            objs.push(BatchRespObject {
                oid: o.oid.clone(),
                size: o.size,
                authenticated: None,
                actions: None,
                error: Some(LfsError {
                    code: 404,
                    message: "object not found".into(),
                }),
            });
            continue;
        }
        objs.push(BatchRespObject {
            oid: o.oid.clone(),
            size: o.size,
            authenticated: None,
            actions: Some(actions),
            error: None,
        });
    }
    let batch_resp = BatchResponse {
        transfer: "basic",
        objects: objs,
    };
    let json = serde_json::to_vec(&batch_resp).map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut resp = (StatusCode::OK, json).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/vnd.git-lfs+json".parse().unwrap(),
    );
    Ok(resp)
}

/// `GET|HEAD /{repo}/info/lfs/objects/{oid}` — stream the object with the full
/// immutable-object contract (strong ETag, 304, Range/If-Range, HEAD,
/// Content-Length); see `static_object`. LFS objects are sha256-addressed.
pub(crate) async fn get_object(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    Path(ObjectPath { oid }): Path<ObjectPath>,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    if !st.cfg.lfs.enabled {
        return Err(ApiError::NotFound("lfs disabled".into()));
    }
    let _ = st
        .auth
        .require_read(req.headers(), route.id.owner(), route.id.name())
        .await?;
    require_lfs_oid(&oid)?;
    let handle = open_repo(&st, &route.id, false).await?;
    let store = handle.store().clone();
    let key = keys::lfs_key(&oid);
    match crate::static_object::serve(
        &store,
        &key,
        req.method(),
        req.headers(),
        crate::static_object::ServeOptions {
            accel: st.cfg.server.accel_redirect,
            peer: crate::request_peer(&req),
            ..Default::default()
        },
    )
    .await
    {
        // The store key is ours, not the client's: name the object as git-lfs knows it.
        Err(ApiError::NotFound(_)) => Err(ApiError::NotFound(format!(
            "LFS object {oid} is not in {}",
            route.id
        ))),
        r => r,
    }
}

/// `PUT /{repo}/info/lfs/objects/{oid}` — stream upload, verify size + sha256.
pub(crate) async fn put_object(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    Path(ObjectPath { oid }): Path<ObjectPath>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    if !st.cfg.lfs.enabled {
        return Err(ApiError::NotFound("lfs disabled".into()));
    }
    let _ = st
        .auth
        .require_write(&headers, route.id.owner(), route.id.name())
        .await?;
    require_lfs_oid(&oid)?;
    let handle = open_repo(&st, &route.id, false).await?;
    let store = handle.store().clone();
    let key = keys::lfs_key(&oid);
    let max = st.cfg.lfs.max_object_bytes.as_u64();

    let tmp = tempfile::NamedTempFile::new().map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut file = tokio::fs::File::from_std(
        tmp.reopen()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    let mut reader = body_to_async_read(body);
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut hasher = Sha256::new();
    let mut n = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let k = reader
            .read(&mut buf)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if k == 0 {
            break;
        }
        n += k as u64;
        if n > max {
            return Err(ApiError::PayloadTooLarge);
        }
        hasher.update(&buf[..k]);
        file.write_all(&buf[..k])
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    drop(file);
    if hex::encode(hasher.finalize()) != oid {
        return Err(ApiError::BadRequest("lfs object sha256 mismatch".into()));
    }
    store
        .put(
            &key,
            PutBody::File(tmp.path().to_path_buf()),
            PutMode::Overwrite.into(),
        )
        .await
        .map_err(store_err)?;
    Ok(StatusCode::OK.into_response())
}

/// `POST /{repo}/info/lfs/verify`
pub(crate) async fn verify(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let body_bytes = crate::collect_body(body).await?;
    if !st.cfg.lfs.enabled {
        return Err(ApiError::NotFound("lfs disabled".into()));
    }
    let body: BatchObject = serde_json::from_slice(&body_bytes)
        .map_err(|e| ApiError::BadRequest(format!("invalid lfs verify: {e}")))?;
    require_lfs_oid(&body.oid)?;
    let _ = st
        .auth
        .require_write(&headers, route.id.owner(), route.id.name())
        .await?;
    let handle = open_repo(&st, &route.id, false).await?;
    let store = handle.store().clone();
    let key = keys::lfs_key(&body.oid);
    let meta = store.head(&key).await.map_err(store_err)?;
    match meta {
        Some(m) if m.size == body.size => Ok(StatusCode::OK.into_response()),
        Some(_) => Err(ApiError::BadRequest("lfs size mismatch".into())),
        None => Err(ApiError::NotFound(body.oid.clone())),
    }
}

fn require_lfs_oid(oid: &str) -> Result<(), ApiError> {
    if keys::lfs_oid_ok(oid) {
        Ok(())
    } else {
        Err(ApiError::BadRequest("invalid lfs oid".into()))
    }
}

fn base_url(st: &AppState, route: &RepoRoute, headers: &HeaderMap) -> String {
    format!(
        "{}/{}",
        crate::smart::request_base_url(st, headers),
        route.id
    )
}

fn store_err(e: gitcask_store::StoreError) -> ApiError {
    e.into()
}
