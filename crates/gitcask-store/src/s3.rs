//! S3-compatible backend (rustfs for local dev / CI).
//!
//! Uses `aws-sdk-s3` for all operations. GET responses are streamed via
//! presigned URLs + `reqwest` because the SDK's `GetObjectOutput::body()`
//! returns `&ByteStream` with no owned-body extractor. All other operations
//! (PUT, HEAD, DELETE, LIST) use the SDK directly.
//!
//! ## Version tokens
//!
//! S3 ETags are used as opaque `Version` strings. Quotes are stripped
//! consistently on read and never stored. For non-multipart uploads the
//! ETag is the MD5 of the content; for multipart uploads it is a compound
//! hash. Callers never parse the token — equality comparison suffices.
//!
//! ## Conditional PUT
//!
//! `PutMode::Create`    → `If-None-Match: *`  (object must not exist).
//! `PutMode::Update(v)` → `If-Match: <etag>`  (CAS on current ETag).
//! On failure the SDK returns a `PreconditionFailed` service error; we fill
//! `current` via a follow-up HEAD when the SDK doesn't include it.
//!
//! ## Conditional DELETE
//!
//! S3 has no native conditional delete. We emulate via HEAD (read ETag) +
//! compare + DELETE, documenting the inherent check-then-act race: a
//! concurrent writer could replace the object between HEAD and DELETE.
//! Acceptable for gitcask's lease-guarded semantics.
//!
//! ## Multipart upload
//!
//! Objects above `cfg.multipart_threshold` use CreateMultipartUpload +
//! UploadPart + CompleteMultipartUpload. CreateMultipartUpload does NOT
//! support `If-None-Match`/`If-Match` in the S3 API, so multipart is only
//! used for `PutMode::Overwrite`. For gitcask's immutable pack objects
//! (`PutMode::Create`) we use single-shot PUT when the object is large,
//! accepting the (tiny) risk of concurrent create races. CAS-rewritten
//! objects (manifests and leases) are always small → single-shot
//! PUT with conditional headers.
//!
//! ## rustfs compatibility (tested with rustfs/rustfs:latest)
//!
//! See the compatibility notes at the bottom of this file.

use std::ops::Range;
use std::time::{Duration, SystemTime};

use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream as S3ByteStream;
use aws_smithy_types::{DateTime, date_time::Format};
use bytes::Bytes;
use futures::StreamExt;

use crate::{
    BoxStream, GetOptions, GetResult, ObjectMeta, ObjectStore, PutBody, PutMode, PutOptions,
    Result, StoreError, Version, util,
};

fn smithy_system_time(value: Option<&DateTime>) -> SystemTime {
    value
        .copied()
        .and_then(|value| SystemTime::try_from(value).ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn http_last_modified(headers: &reqwest::header::HeaderMap) -> SystemTime {
    headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| DateTime::from_str(value, Format::HttpDate).ok())
        .and_then(|value| SystemTime::try_from(value).ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// S3-compatible object store.
pub struct S3Store {
    client: S3Client,
    bucket: String,
    /// reqwest client for streaming GETs via presigned URLs.
    http: reqwest::Client,
    multipart_threshold: u64,
    multipart_part_size: u64,
    max_retries: u32,
}

impl S3Store {
    /// Build a store from `gitcask-config::StoreConfig`.
    ///
    /// Credentials are read from the env vars named in
    /// `cfg.s3.access_key_env` / `cfg.s3.secret_key_env`
    /// (defaults `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), plus
    /// `AWS_SESSION_TOKEN` when present.
    pub async fn new(cfg: &gitcask_config::StoreConfig) -> anyhow::Result<Self> {
        let access_key = std::env::var(&cfg.s3.access_key_env).map_err(|_| {
            anyhow::anyhow!("s3: env var {} not set (access key)", cfg.s3.access_key_env)
        })?;
        let secret_key = std::env::var(&cfg.s3.secret_key_env).map_err(|_| {
            anyhow::anyhow!("s3: env var {} not set (secret key)", cfg.s3.secret_key_env)
        })?;

        let creds = static_credentials(
            &access_key,
            &secret_key,
            std::env::var("AWS_SESSION_TOKEN").ok(),
        );
        let region = aws_sdk_s3::config::Region::new(cfg.s3.region.clone());

        let mut s3_config = aws_sdk_s3::Config::builder()
            .region(region)
            .credentials_provider(creds)
            .force_path_style(cfg.s3.force_path_style)
            .behavior_version_latest();

        if !cfg.s3.endpoint.is_empty() {
            s3_config = s3_config.endpoint_url(&cfg.s3.endpoint);
        }

        let client = S3Client::from_conf(s3_config.build());
        let http = reqwest::Client::builder().build()?;

        Ok(S3Store {
            client,
            bucket: cfg.bucket.clone(),
            http,
            multipart_threshold: cfg.multipart_threshold.as_u64(),
            multipart_part_size: cfg.multipart_part_size.as_u64(),
            max_retries: cfg.max_retries,
        })
    }

    /// `bytes=start-(end-1)` for a half-open range (S3 Range is inclusive).
    fn range_header(range: &Range<u64>) -> String {
        format!("bytes={}-{}", range.start, range.end.saturating_sub(1))
    }

    // ---- GET via presigned URL + reqwest (true streaming) ---------------

    async fn presigned_get(&self, key: &str, opts: &GetOptions) -> Result<reqwest::Response> {
        let presigning = PresigningConfig::expires_in(Duration::from_secs(60))
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning config: {e}")))?;

        let mut builder = self.client.get_object().bucket(&self.bucket).key(key);

        if let Some(v) = &opts.if_none_match {
            builder = builder.if_none_match(v.as_str());
        }
        if let Some(v) = &opts.if_match {
            builder = builder.if_match(v.as_str());
        }
        if let Some(r) = &opts.range {
            builder = builder.range(Self::range_header(r));
        }

        let presigned = builder
            .presigned(presigning)
            .await
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning get: {e}")))?;

        let mut req = self.http.get(presigned.uri());
        for (name, value) in presigned.headers() {
            req = req.header(name, value);
        }

        req.send()
            .await
            .map_err(|e| classify("get", key, S3Failure::Http(e)))
    }

    fn get_result_from_response(key: &str, resp: reqwest::Response) -> Result<GetResult> {
        let status = resp.status();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned());
        let content_length = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        // `ObjectMeta::size` is the size of the whole object (as in memory tests),
        // also for range reads: `Content-Range: bytes a-b/total` carries it.
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, t)| t.trim().parse::<u64>().ok());

        match status.as_u16() {
            200 | 206 => {
                let version = Version::new(etag.as_deref().unwrap_or(""));
                let meta = ObjectMeta {
                    key: key.into(),
                    size: total.or(content_length).unwrap_or(0),
                    version,
                    last_modified: http_last_modified(resp.headers()),
                };
                let key_owned = key.to_owned();
                let body = resp
                    .bytes_stream()
                    .map(move |r| {
                        r.map_err(|e| classify("get_body", &key_owned, S3Failure::Http(e)))
                    })
                    .boxed();
                Ok(GetResult::Object { meta, body })
            }
            304 => Ok(GetResult::NotModified {
                version: Version::new(etag.as_deref().unwrap_or("")),
            }),
            404 => Err(StoreError::NotFound { key: key.into() }),
            412 => Err(StoreError::PreconditionFailed {
                key: key.into(),
                current: etag.map(|e| Version::new(e)),
            }),
            s => Err(classify("get", key, S3Failure::Status(s))),
        }
    }
}

// ---- PutBody → SDK ByteStream ------------------------------------------

async fn body_to_s3(body: PutBody) -> Result<(S3ByteStream, u64)> {
    Ok(match body {
        PutBody::Bytes(b) => {
            let len = b.len() as u64;
            (S3ByteStream::from(b), len)
        }
        PutBody::Stream { len, stream } => {
            // Collect into Bytes: gitcask's Stream bodies are small objects
            // (manifests, leases). Large packs use PutBody::File which
            // streams via ByteStream::read_from().
            let collected = util::collect(stream, len as usize).await?;
            (S3ByteStream::from(collected), len)
        }
        PutBody::File(path) => {
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| StoreError::other(anyhow::anyhow!("stat {}: {e}", path.display())))?;
            let len = meta.len();
            let stream = S3ByteStream::read_from()
                .path(&path)
                .buffer_size(64 * 1024)
                .build()
                .await
                .map_err(|e| StoreError::other(anyhow::anyhow!("file stream: {e}")))?;
            (stream, len)
        }
    })
}

// ---- error classification ----------------------------------------------

#[derive(Clone, Copy)]
enum SdkFailureKind {
    Service,
    Dispatch,
    Timeout,
    Response,
    Construction,
}

enum S3Failure {
    Sdk {
        code: Option<String>,
        status: Option<u16>,
        kind: SdkFailureKind,
        message: String,
    },
    Http(reqwest::Error),
    Status(u16),
}

fn sdk_failure<E>(err: SdkError<E>) -> S3Failure
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let code = err.code().map(str::to_owned);
    let status = err.raw_response().map(|r| r.status().as_u16());
    let kind = match &err {
        SdkError::DispatchFailure(_) => SdkFailureKind::Dispatch,
        SdkError::TimeoutError(_) => SdkFailureKind::Timeout,
        SdkError::ResponseError(_) => SdkFailureKind::Response,
        SdkError::ConstructionFailure(_) => SdkFailureKind::Construction,
        _ => SdkFailureKind::Service,
    };
    S3Failure::Sdk {
        code,
        status,
        kind,
        message: DisplayErrorContext(err).to_string(),
    }
}

fn classify(op: &'static str, key: &str, failure: S3Failure) -> StoreError {
    let (code, status, transport, message) = match failure {
        S3Failure::Sdk {
            code,
            status,
            kind,
            message,
        } => (
            code,
            status,
            matches!(
                kind,
                SdkFailureKind::Dispatch | SdkFailureKind::Timeout | SdkFailureKind::Response
            ),
            message,
        ),
        S3Failure::Http(err) => {
            let retryable = !err.is_builder()
                && (err.is_connect()
                    || err.is_timeout()
                    || err.is_body()
                    || err.is_decode()
                    || err.is_request());
            (
                None,
                err.status().map(|s| s.as_u16()),
                retryable,
                err.to_string(),
            )
        }
        S3Failure::Status(status) => (None, Some(status), false, format!("HTTP {status}")),
    };
    let detail = anyhow::anyhow!("s3 {op} {key}: {message}");
    if matches!(
        code.as_deref(),
        Some("PreconditionFailed" | "ConditionalRequestConflict")
    ) || status == Some(412)
    {
        StoreError::PreconditionFailed {
            key: key.into(),
            current: None,
        }
    } else if matches!(code.as_deref(), Some("NotFound" | "NoSuchKey")) || status == Some(404) {
        StoreError::NotFound { key: key.into() }
    } else if transport
        || matches!(status, Some(429 | 500 | 502 | 503 | 504))
        || matches!(
            code.as_deref(),
            Some(
                "SlowDown"
                    | "InternalError"
                    | "RequestTimeout"
                    | "ServiceUnavailable"
                    | "Throttling"
                    | "ThrottlingException"
                    | "RequestLimitExceeded"
                    | "BadDigest"
            )
        )
    {
        StoreError::Retryable(detail)
    } else {
        StoreError::Other(detail)
    }
}

impl S3Store {
    async fn single_put_once(
        &self,
        key: &str,
        body: PutBody,
        opts: &PutOptions,
    ) -> Result<ObjectMeta> {
        let (s3_body, len) = body_to_s3(body).await?;
        let mut builder = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(s3_body)
            .content_length(len as i64);

        match &opts.mode {
            PutMode::Overwrite => {}
            PutMode::Create => builder = builder.if_none_match("*"),
            PutMode::Update(v) => builder = builder.if_match(v.as_str()),
        }
        if let Some(ct) = opts.content_type {
            builder = builder.content_type(ct);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| classify("put", key, sdk_failure(e)))?;
        let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
        Ok(ObjectMeta {
            key: key.into(),
            size: len,
            version: Version::new(etag.as_deref().unwrap_or("")),
            last_modified: SystemTime::now(),
        })
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    fn backend(&self) -> &'static str {
        "s3"
    }

    async fn get(&self, key: &str, opts: GetOptions) -> Result<GetResult> {
        util::with_retry("get", key, self.max_retries, || async {
            let resp = self.presigned_get(key, &opts).await?;
            Self::get_result_from_response(key, resp)
        })
        .await
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        util::with_retry("head", key, self.max_retries, || async {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(out) => {
                    let etag = out.e_tag().map(|s| s.trim_matches('"').to_owned());
                    let size = out.content_length().unwrap_or(0) as u64;
                    Ok(Some(ObjectMeta {
                        key: key.into(),
                        size,
                        version: Version::new(etag.as_deref().unwrap_or("")),
                        last_modified: smithy_system_time(out.last_modified()),
                    }))
                }
                Err(err) => match classify("head", key, sdk_failure(err)) {
                    StoreError::NotFound { .. } => Ok(None),
                    error => Err(error),
                },
            }
        })
        .await
    }

    async fn put(&self, key: &str, body: PutBody, opts: PutOptions) -> Result<ObjectMeta> {
        let len = match &body {
            PutBody::Bytes(bytes) => bytes.len() as u64,
            PutBody::Stream { len, .. } => *len,
            PutBody::File(path) => tokio::fs::metadata(path)
                .await
                .map_err(|e| StoreError::other(anyhow::anyhow!("stat {}: {e}", path.display())))?
                .len(),
        };

        // Multipart only for Overwrite (CreateMultipartUpload has no
        // conditional header support in the S3 API). Create/Update always
        // use single-shot PUT.
        let use_multipart =
            len > self.multipart_threshold && matches!(opts.mode, PutMode::Overwrite);

        if use_multipart {
            let retry_limit = if matches!(body, PutBody::Stream { .. }) {
                0
            } else {
                self.max_retries
            };
            let (s3_body, _) = body_to_s3(body).await?;
            return self
                .multipart_put(key, s3_body, len, &opts, retry_limit)
                .await;
        }

        // A retried Create/Update may return 412 after the first attempt actually
        // landed. We cannot prove that the current bytes are ours without an
        // extra read, so conditional PUTs get one attempt. coord::cas_update
        // already retries Retryable manifest CAS failures with its own backoff.
        // Streams are likewise one-shot; Bytes are cloned and Files are reopened
        // by `body_to_s3` for every Overwrite attempt.
        let retries =
            if matches!(opts.mode, PutMode::Overwrite) && !matches!(body, PutBody::Stream { .. }) {
                self.max_retries
            } else {
                0
            };
        let result = match body {
            PutBody::Bytes(bytes) => {
                util::with_retry("put", key, retries, || {
                    self.single_put_once(key, PutBody::Bytes(bytes.clone()), &opts)
                })
                .await
            }
            PutBody::File(path) => {
                util::with_retry("put", key, retries, || {
                    self.single_put_once(key, PutBody::File(path.clone()), &opts)
                })
                .await
            }
            PutBody::Stream { len, stream } => {
                self.single_put_once(key, PutBody::Stream { len, stream }, &opts)
                    .await
            }
        };

        match result {
            Err(StoreError::PreconditionFailed {
                key: failed,
                current,
            }) if current.is_none() => {
                let current = self.head(key).await.ok().flatten().map(|m| m.version);
                Err(StoreError::PreconditionFailed {
                    key: failed,
                    current,
                })
            }
            other => other,
        }
    }

    async fn delete(&self, key: &str, if_version: Option<Version>) -> Result<()> {
        if let Some(want) = &if_version {
            // S3 has no conditional delete: emulate via HEAD + compare + DELETE.
            // RACE: a concurrent writer could replace the object between HEAD
            // and DELETE. Acceptable for gitcask's lease-guarded semantics.
            let head = self.head(key).await?;
            match head {
                None => return Err(StoreError::NotFound { key: key.into() }),
                Some(meta) if &meta.version != want => {
                    return Err(StoreError::PreconditionFailed {
                        key: key.into(),
                        current: Some(meta.version),
                    });
                }
                _ => {}
            }
        }

        util::with_retry("delete", key, self.max_retries, || async {
            match self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(_) => Ok(()),
                Err(err) => match classify("delete", key, sdk_failure(err)) {
                    StoreError::NotFound { .. } if if_version.is_none() => Ok(()),
                    error => Err(error),
                },
            }
        })
        .await
    }

    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let prefix = prefix.to_owned();
        let start_after = start_after.map(|s| s.to_owned());
        let max_retries = self.max_retries;

        Box::pin(futures::stream::unfold(
            ListState {
                client,
                bucket,
                prefix,
                start_after,
                continuation_token: None,
                started: false,
                buffer: Vec::new().into_iter(),
            },
            move |mut state| async move {
                // Drain buffered items first.
                if let Some(item) = state.buffer.next() {
                    return Some((item, state));
                }

                if state.started && state.continuation_token.is_none() {
                    return None;
                }
                state.started = true;

                let mut builder = state
                    .client
                    .list_objects_v2()
                    .bucket(&state.bucket)
                    .prefix(&state.prefix)
                    .max_keys(1000);

                if let Some(sa) = &state.start_after {
                    builder = builder.start_after(sa);
                }
                if let Some(ct) = &state.continuation_token {
                    builder = builder.continuation_token(ct);
                }

                let page = util::with_retry("list", &state.prefix, max_retries, || async {
                    builder
                        .clone()
                        .send()
                        .await
                        .map_err(|err| classify("list", &state.prefix, sdk_failure(err)))
                })
                .await;

                match page {
                    Ok(resp) => {
                        let items: Vec<Result<ObjectMeta>> = resp
                            .contents()
                            .iter()
                            .map(|obj| {
                                let etag = obj.e_tag().map(|s| s.trim_matches('"').to_owned());
                                Ok(ObjectMeta {
                                    key: obj.key().unwrap_or("").to_owned(),
                                    size: obj.size().unwrap_or(0) as u64,
                                    version: Version::new(etag.as_deref().unwrap_or("")),
                                    last_modified: smithy_system_time(obj.last_modified()),
                                })
                            })
                            .collect();

                        state.continuation_token = resp
                            .is_truncated()
                            .unwrap_or(false)
                            .then(|| resp.next_continuation_token().map(|s| s.to_owned()))
                            .flatten();
                        state.buffer = items.into_iter();

                        let item = state.buffer.next();
                        item.map(|i| (i, state))
                    }
                    Err(err) => Some((Err(err), state)),
                }
            },
        ))
    }

    async fn list_prefixes(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut continuation_token: Option<String> = None;
        loop {
            let mut builder = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .delimiter("/")
                .max_keys(1000);
            if let Some(ct) = &continuation_token {
                builder = builder.continuation_token(ct);
            }
            let resp = util::with_retry("list_prefixes", prefix, self.max_retries, || async {
                builder
                    .clone()
                    .send()
                    .await
                    .map_err(|err| classify("list_prefixes", prefix, sdk_failure(err)))
            })
            .await?;
            out.extend(
                resp.common_prefixes()
                    .iter()
                    .filter_map(|p| p.prefix().map(str::to_owned)),
            );
            continuation_token = resp
                .is_truncated()
                .unwrap_or(false)
                .then(|| resp.next_continuation_token().map(|s| s.to_owned()))
                .flatten();
            if continuation_token.is_none() {
                break;
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// A presigned GET (1 h): the edge needs no credentials and `Range` stays free (unsigned).
    async fn accel_target(&self, key: &str) -> Option<crate::AccelTarget> {
        let url = self
            .signed_get_url(key, Duration::from_secs(3600))
            .await
            .ok()
            .flatten()?;
        Some(crate::AccelTarget {
            url,
            authorization: None,
        })
    }

    fn supports_compose(&self) -> bool {
        true
    }

    /// Concatenate `sources` into `dest` with one multipart upload whose parts are
    /// `UploadPartCopy` byte ranges of the sources — nothing streams through this process
    /// except the parts S3 will not copy: every part but the last must be >= 5 MiB, so a
    /// small source in front of a large object is fetched and uploaded
    /// together with the beginning of the next source as one ordinary part.
    async fn compose(
        &self,
        dest: &str,
        sources: &[String],
        opts: PutOptions,
    ) -> Result<ObjectMeta> {
        const MIN_PART: u64 = 5 * 1024 * 1024;
        const COPY_PART: u64 = 1024 * 1024 * 1024; // <= 5 GiB per UploadPartCopy
        if sources.is_empty() {
            return Err(StoreError::InvalidArgument(
                "compose needs at least one source".into(),
            ));
        }
        if let PutMode::Create = opts.mode
            && self.head(dest).await?.is_some()
        {
            return Err(StoreError::PreconditionFailed {
                key: dest.to_owned(),
                current: None,
            });
        }
        // Sizes first: the layout of parts depends on them.
        let mut sizes = Vec::with_capacity(sources.len());
        for src in sources {
            let m = self
                .head(src)
                .await?
                .ok_or_else(|| StoreError::NotFound { key: src.clone() })?;
            sizes.push(m.size);
        }
        let total: u64 = sizes.iter().sum();
        // The virtual concatenation, cut into parts: a part is [start, end) of the whole.
        // Runs that lie inside one source and are >= MIN_PART become copies; everything else
        // (a small source, the tail that pads it to MIN_PART) is read and uploaded.
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(dest);
        if let Some(ct) = opts.content_type {
            create = create.content_type(ct);
        }
        if opts.immutable {
            create = create.cache_control("public, max-age=31536000, immutable");
        }
        let upload = util::with_retry("multipart_create", dest, self.max_retries, || async {
            create
                .clone()
                .send()
                .await
                .map_err(|e| classify("multipart_create", dest, sdk_failure(e)))
        })
        .await?;
        let upload_id = upload
            .upload_id()
            .ok_or_else(|| {
                StoreError::other(anyhow::anyhow!("no upload_id from CreateMultipartUpload"))
            })?
            .to_owned();
        let mut parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
        let mut part_number = 1i32;
        let mut pos: u64 = 0; // absolute offset into the concatenation
        let offset_of = |i: usize| -> u64 { sizes[..i].iter().sum() };
        let result: Result<()> = async {
            while pos < total {
                // Which source does `pos` fall in, and how far does it run?
                let i = (0..sources.len())
                    .find(|&i| pos < offset_of(i) + sizes[i])
                    .unwrap();
                let src_end = offset_of(i) + sizes[i];
                let run = src_end - pos;
                let last_part = src_end == total;
                if run >= MIN_PART || last_part {
                    // Copy a range of this one source.
                    let len = run.min(COPY_PART);
                    let from = pos - offset_of(i);
                    let part =
                        util::with_retry("upload_part_copy", dest, self.max_retries, || async {
                            self.client
                                .upload_part_copy()
                                .bucket(&self.bucket)
                                .key(dest)
                                .upload_id(&upload_id)
                                .part_number(part_number)
                                .copy_source(format!(
                                    "{}/{}",
                                    self.bucket,
                                    crate::util::encode_path(&sources[i])
                                ))
                                .copy_source_range(format!("bytes={from}-{}", from + len - 1))
                                .send()
                                .await
                                .map_err(|e| classify("upload_part_copy", dest, sdk_failure(e)))
                        })
                        .await?;
                    let etag = part
                        .copy_part_result()
                        .and_then(|r| r.e_tag())
                        .unwrap_or("")
                        .to_owned();
                    parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .e_tag(etag)
                            .part_number(part_number)
                            .build(),
                    );
                    pos += len;
                } else {
                    // Too small to copy on its own: read MIN_PART bytes across source boundaries.
                    let want = MIN_PART.min(total - pos);
                    let mut buf = Vec::with_capacity(want as usize);
                    let mut p = pos;
                    while (buf.len() as u64) < want {
                        let j = (0..sources.len())
                            .find(|&j| p < offset_of(j) + sizes[j])
                            .unwrap();
                        let from = p - offset_of(j);
                        let take = (sizes[j] - from).min(want - buf.len() as u64);
                        let (_, bytes) = self
                            .get(
                                &sources[j],
                                GetOptions {
                                    range: Some(from..from + take),
                                    ..GetOptions::default()
                                },
                            )
                            .await?
                            .bytes()
                            .await?
                            .ok_or_else(|| StoreError::NotFound {
                                key: sources[j].clone(),
                            })?;
                        buf.extend_from_slice(&bytes);
                        p += take;
                    }
                    let len = buf.len() as u64;
                    let bytes = Bytes::from(buf);
                    let part = util::with_retry("upload_part", dest, self.max_retries, || async {
                        self.client
                            .upload_part()
                            .bucket(&self.bucket)
                            .key(dest)
                            .upload_id(&upload_id)
                            .part_number(part_number)
                            .body(S3ByteStream::from(bytes.clone()))
                            .content_length(len as i64)
                            .send()
                            .await
                            .map_err(|e| classify("upload_part", dest, sdk_failure(e)))
                    })
                    .await?;
                    parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .e_tag(part.e_tag().unwrap_or("").to_owned())
                            .part_number(part_number)
                            .build(),
                    );
                    pos += len;
                }
                part_number += 1;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = self.abort_multipart(dest, &upload_id).await;
            return Err(e);
        }
        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        // A lost Complete response is ambiguous for a conditional compose, so
        // do not replay it. The caller receives Retryable and re-plans safely.
        let resp = match util::with_retry("multipart_complete", dest, 0, || async {
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(dest)
                .upload_id(&upload_id)
                .multipart_upload(completed.clone())
                .send()
                .await
                .map_err(|e| classify("multipart_complete", dest, sdk_failure(e)))
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self.abort_multipart(dest, &upload_id).await;
                return Err(e);
            }
        };
        let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
        Ok(ObjectMeta {
            key: dest.into(),
            size: total,
            version: Version::new(etag.as_deref().unwrap_or("")),
            last_modified: SystemTime::now(),
        })
    }

    async fn signed_get_url(&self, key: &str, ttl: Duration) -> Result<Option<String>> {
        let presigning = PresigningConfig::expires_in(ttl)
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning config: {e}")))?;
        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning)
            .await
            .map_err(|e| StoreError::other(anyhow::anyhow!("presigning: {e}")))?;
        Ok(Some(presigned.uri().to_owned()))
    }
}

/// State for the lazy list stream.
struct ListState {
    client: S3Client,
    bucket: String,
    prefix: String,
    start_after: Option<String>,
    continuation_token: Option<String>,
    started: bool,
    buffer: std::vec::IntoIter<Result<ObjectMeta>>,
}

// ---- multipart upload (Overwrite only) ---------------------------------

impl S3Store {
    async fn multipart_put(
        &self,
        key: &str,
        body: S3ByteStream,
        len: u64,
        opts: &PutOptions,
        retry_limit: u32,
    ) -> Result<ObjectMeta> {
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);

        if let Some(ct) = opts.content_type {
            create = create.content_type(ct);
        }

        let upload = util::with_retry("multipart_create", key, retry_limit, || async {
            create
                .clone()
                .send()
                .await
                .map_err(|e| classify("multipart_create", key, sdk_failure(e)))
        })
        .await?;

        let upload_id = upload
            .upload_id()
            .ok_or_else(|| {
                StoreError::other(anyhow::anyhow!("no upload_id from CreateMultipartUpload"))
            })?
            .to_owned();

        let part_size = self.multipart_part_size;
        let mut part_number = 1i32;
        let mut uploaded_parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
        let mut remaining = len;

        use tokio::io::AsyncReadExt;
        let mut reader = body.into_async_read();

        while remaining > 0 {
            let this_part = part_size.min(remaining);
            let to_read = this_part as usize;
            let mut buf = vec![0u8; to_read];
            let mut read_total = 0;

            while read_total < to_read {
                let n = match reader.read(&mut buf[read_total..]).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = self
                            .abort_multipart_with_limit(key, &upload_id, retry_limit)
                            .await;
                        return Err(StoreError::other(anyhow::anyhow!("multipart read: {e}")));
                    }
                };
                if n == 0 {
                    break;
                }
                read_total += n;
            }

            if read_total == 0 {
                break;
            }
            buf.truncate(read_total);
            let actual = read_total as u64;

            let bytes = Bytes::from(buf);
            let part = match util::with_retry("upload_part", key, retry_limit, || async {
                self.client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(S3ByteStream::from(bytes.clone()))
                    .content_length(actual as i64)
                    .send()
                    .await
                    .map_err(|e| classify("upload_part", key, sdk_failure(e)))
            })
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    let _ = self
                        .abort_multipart_with_limit(key, &upload_id, retry_limit)
                        .await;
                    return Err(e);
                }
            };

            let etag = part.e_tag().unwrap_or("").to_owned();
            uploaded_parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .e_tag(etag)
                    .part_number(part_number)
                    .build(),
            );

            remaining -= actual;
            part_number += 1;
        }

        let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(uploaded_parts))
            .build();

        let resp = match util::with_retry("multipart_complete", key, retry_limit, || async {
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(completed.clone())
                .send()
                .await
                .map_err(|e| classify("multipart_complete", key, sdk_failure(e)))
        })
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = self
                    .abort_multipart_with_limit(key, &upload_id, retry_limit)
                    .await;
                return Err(e);
            }
        };

        let etag = resp.e_tag().map(|s| s.trim_matches('"').to_owned());
        Ok(ObjectMeta {
            key: key.into(),
            size: len,
            version: Version::new(etag.as_deref().unwrap_or("")),
            last_modified: SystemTime::now(),
        })
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        self.abort_multipart_with_limit(key, upload_id, self.max_retries)
            .await
    }

    async fn abort_multipart_with_limit(
        &self,
        key: &str,
        upload_id: &str,
        retry_limit: u32,
    ) -> Result<()> {
        util::with_retry("multipart_abort", key, retry_limit, || async {
            self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| classify("multipart_abort", key, sdk_failure(e)))
        })
        .await
    }
}

fn static_credentials(
    access_key: &str,
    secret_key: &str,
    session_token: Option<String>,
) -> Credentials {
    Credentials::new(
        access_key,
        secret_key,
        session_token,
        None,
        "gitcask-static",
    )
}

// ---- rustfs compatibility notes (integration testing) -------------------
//
// 1. Presigned URLs: rustfs honors SigV4 presigned GET URLs with conditional
//    headers (If-None-Match, If-Match, Range) in SignedHeaders.
// 2. If-None-Match: * on PUT: 412 "PreconditionFailed" when object exists.
// 3. If-Match: <etag> on PUT: 412 when ETag mismatch.
// 4. 304 Not Modified: HTTP 304 with ETag header, empty body.
// 5. ListObjectsV2: StartAfter, ContinuationToken, IsTruncated/NextToken OK.
// 6. DeleteObject: idempotent for absent keys (204).
// 7. Multipart: CreateMultipartUpload + UploadPart + CompleteMultipartUpload
//    supported. No conditional headers on Create/Complete (same as real S3).
// 8. ETags: quoted, MD5 for single-PUT, compound for multipart. Quotes
//    stripped consistently in our Version.
// 9. force_path_style: required for rustfs local dev.

#[cfg(test)]
mod tests {
    use super::*;

    fn sdk_failure_for_test(
        code: Option<&str>,
        status: Option<u16>,
        kind: SdkFailureKind,
    ) -> S3Failure {
        S3Failure::Sdk {
            code: code.map(str::to_owned),
            status,
            kind,
            message: "injected".into(),
        }
    }

    #[test]
    fn transient_statuses_and_codes_are_retryable() {
        for status in [429, 500, 502, 503, 504] {
            assert!(classify("put", "key", S3Failure::Status(status)).is_retryable());
        }
        for code in [
            "SlowDown",
            "InternalError",
            "RequestTimeout",
            "ServiceUnavailable",
            "Throttling",
            "ThrottlingException",
            "RequestLimitExceeded",
            "BadDigest",
        ] {
            let error = classify(
                "put",
                "key",
                sdk_failure_for_test(Some(code), None, SdkFailureKind::Service),
            );
            assert!(error.is_retryable());
        }
    }

    #[test]
    fn sdk_transport_failures_are_retryable() {
        for kind in [
            SdkFailureKind::Dispatch,
            SdkFailureKind::Timeout,
            SdkFailureKind::Response,
        ] {
            let error = classify("head", "key", sdk_failure_for_test(None, None, kind));
            assert!(error.is_retryable());
        }
    }

    #[test]
    fn permanent_s3_failures_keep_their_meaning() {
        let precondition = classify(
            "put",
            "key",
            sdk_failure_for_test(
                Some("ConditionalRequestConflict"),
                Some(409),
                SdkFailureKind::Service,
            ),
        );
        assert!(precondition.is_precondition_failed());

        let not_found = classify("head", "key", S3Failure::Status(404));
        assert!(not_found.is_not_found());

        let other = classify("put", "key", S3Failure::Status(400));
        assert!(matches!(other, StoreError::Other(_)));
    }

    #[test]
    fn static_credentials_include_session_token_when_present() {
        let creds = static_credentials("access", "secret", Some("session".into()));

        assert_eq!(creds.access_key_id(), "access");
        assert_eq!(creds.secret_access_key(), "secret");
        assert_eq!(creds.session_token(), Some("session"));
    }

    #[test]
    fn static_credentials_work_without_session_token() {
        let creds = static_credentials("access", "secret", None);

        assert_eq!(creds.access_key_id(), "access");
        assert_eq!(creds.secret_access_key(), "secret");
        assert_eq!(creds.session_token(), None);
    }
}
