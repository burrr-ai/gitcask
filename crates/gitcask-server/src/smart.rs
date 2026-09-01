//! Git smart HTTP protocol (v0/v2): info/refs, upload-pack, receive-pack.
//!
//! References:
//! * https://git-scm.com/docs/http-protocol
//! * https://git-scm.com/docs/protocol-v2

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::ApiError;
use crate::repo::RepoRoute;
use crate::stream::{VecWriter, body_to_async_read, maybe_gunzip, write_body_pipe};
use gitcask_git::pkt as pktline;
use tracing::Instrument;

/// Cache-control headers for smart endpoints (info/refs and pkt responses).
fn no_cache_headers() -> [(axum::http::HeaderName, &'static str); 3] {
    [
        (
            axum::http::header::CACHE_CONTROL,
            "no-cache, max-age=0, must-revalidate",
        ),
        (axum::http::header::EXPIRES, "Fri, 01 Jan 1980 00:00:00 GMT"),
        (axum::http::header::PRAGMA, "no-cache"),
    ]
}

pub(crate) async fn info_refs_route(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let _permit = st.semaphores.acquire(&route.id.to_string()).await;
    info_refs(&st, &route, &headers, query.as_deref().unwrap_or_default()).await
}

pub(crate) async fn upload_pack_route(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let _permit = st.semaphores.acquire(&route.id.to_string()).await;
    upload_pack(&st, &route, &headers, body).await
}

pub(crate) async fn receive_pack_route(
    State(st): State<Arc<AppState>>,
    route: RepoRoute,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let _permit = st.semaphores.acquire(&route.id.to_string()).await;
    receive_pack(&st, &route, &headers, body).await
}

/// `GET /{owner}/{repo}[.git]/info/refs?service=git-upload-pack|git-receive-pack`
pub async fn info_refs(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    query: &str,
) -> Result<Response, ApiError> {
    let service_param = parse_query(query, "service").unwrap_or_default();
    // Auth: read for upload-pack, write for receive-pack advertisement.
    let is_receive = service_param == "git-receive-pack";
    let auth_result = if is_receive {
        st.auth
            .require_write(headers, route.id.owner(), route.id.name())
            .await
    } else {
        st.auth
            .require_read(headers, route.id.owner(), route.id.name())
            .await
    };
    if let Err(e) = auth_result {
        return Err(e.into());
    }
    if is_receive {
        if let Some(msg) = push_url_must_be_git(st, route, headers) {
            return Ok(git_err_response("git-receive-pack", &msg));
        }
    }

    let service = match service_param.as_str() {
        "git-upload-pack" => gitcask_git::Service::UploadPack,
        "git-receive-pack" => gitcask_git::Service::ReceivePack,
        other => return Err(ApiError::BadRequest(format!("unknown service: {other}"))),
    };

    let handle = open_repo(st, &route.id, is_receive).await?;
    // Advertisements need refs only: never wait for (or require) the pack set.
    let _guard = handle.sync_refs().await?;

    let protocol = gitcask_git::pkt::Protocol::from_git_protocol_header(
        headers.get("git-protocol").and_then(|v| v.to_str().ok()),
    );

    // Build the response: service header + flush, then advertisement.
    let mut buf = Vec::with_capacity(2048);
    let svc_line = format!("# service={service_param}\n");
    pktline::encode_data(&mut buf, svc_line.as_bytes());
    pktline::encode_flush(&mut buf);

    match (protocol, service) {
        (gitcask_git::pkt::Protocol::V2, gitcask_git::Service::UploadPack) => {
            v2_capability_advert(st, &handle, &mut buf).await?;
        }
        _ => {
            // v0 (and receive-pack always).
            let repo_key = route.id.to_string();
            let ver = handle.manifest_version();
            if let Some(cached) = st
                .caches
                .ref_advert
                .get_v0(&repo_key, ver.as_ref(), service)
            {
                buf.extend_from_slice(&cached);
            } else {
                let start = buf.len();
                handle.local().advertise_refs_v0(service, &mut buf)?;
                let advert_bytes = buf[start..].to_vec();
                st.caches
                    .ref_advert
                    .insert_v0(&repo_key, ver.as_ref(), service, advert_bytes);
            }
        }
    }

    let ct = format!("application/x-{service_param}-advertisement");
    Ok(build_response(StatusCode::OK, &ct, no_cache_headers(), buf))
}

fn parse_query(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Build the protocol v2 capability advertisement for upload-pack.
async fn v2_capability_advert(
    st: &AppState,
    handle: &Arc<gitcask_wal::RepoHandle>,
    buf: &mut Vec<u8>,
) -> Result<(), ApiError> {
    let ver = env!("CARGO_PKG_VERSION");
    pktline::encode_data(buf, b"version 2\n");
    pktline::encode_data(buf, format!("agent=gitcask/{ver}\n").as_bytes());
    pktline::encode_data(buf, b"ls-refs=unborn\n");
    let mut fetch = String::from("fetch=shallow wait-for-done");
    if st.cfg.git.allow_filter {
        fetch.push_str(" filter");
    }
    // With sideband-all every response line is sideband-framed, which lets us
    // narrate what the server is doing (band 2 → "remote: * …") *before* the
    // packfile section: auth, WAL sync and materialization progress. Both
    // engines frame their sections that way.
    fetch.push_str(" sideband-all");
    pktline::encode_data(buf, format!("{fetch}\n").as_bytes());
    pktline::encode_data(buf, b"server-option\n");
    let fmt = match handle.local().object_format() {
        gitcask_git::ObjectFormat::Sha1 => "sha1",
        gitcask_git::ObjectFormat::Sha256 => "sha256",
    };
    pktline::encode_data(buf, format!("object-format={fmt}\n").as_bytes());
    pktline::encode_flush(buf);
    Ok(())
}

/// `POST /{owner}/{repo}[.git]/git-upload-pack`
pub async fn upload_pack(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    st.auth
        .require_read(headers, route.id.owner(), route.id.name())
        .await?;

    let handle = open_repo(st, &route.id, false).await?;

    let protocol = gitcask_git::pkt::Protocol::from_git_protocol_header(
        headers.get("git-protocol").and_then(|v| v.to_str().ok()),
    );

    let enc = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let reader = maybe_gunzip(enc, body_to_async_read(body));

    // Synchronization is deferred to each handler path so the ReadGuard lives for the
    // entire streaming response (packs must not be removed mid-clone).
    match protocol {
        gitcask_git::pkt::Protocol::V2 => upload_pack_v2(st, route, headers, &handle, reader).await,
        gitcask_git::pkt::Protocol::V0 => upload_pack_v0(&handle, reader).await,
    }
}

async fn upload_pack_v2(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    handle: &Arc<gitcask_wal::RepoHandle>,
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
) -> Result<Response, ApiError> {
    let (cmd, reader) = gitcask_git::pkt::read_command(reader).await?;
    match cmd.name.as_str() {
        "ls-refs" => {
            let _guard = handle.sync_refs().await?;
            let req = gitcask_git::pkt::parse_ls_refs(&cmd);
            let req = gitcask_git::pkt::read_ls_refs_args(reader, req).await?;
            let args = gitcask_git::LsRefsArgs {
                ref_prefixes: req.prefixes,
                symrefs: req.symrefs,
                peel: req.peel,
                unborn: req.unborn,
            };
            let repo_key = route.id.to_string();
            let version = handle.manifest_version();
            let lines =
                match st
                    .caches
                    .ref_advert
                    .get_v2_ls_refs(&repo_key, version.as_ref(), &args)
                {
                    Some(lines) => lines,
                    None => {
                        let lines = handle.local().ls_refs(&args)?;
                        st.caches.ref_advert.insert_v2_ls_refs(
                            &repo_key,
                            version.as_ref(),
                            &args,
                            lines.clone(),
                        );
                        lines
                    }
                };
            let mut buf = Vec::with_capacity(1024);
            for line in &lines {
                pktline::encode_data(&mut buf, line.render(&args).as_bytes());
            }
            pktline::encode_flush(&mut buf);
            Ok(text_response(
                "application/x-git-upload-pack-result",
                no_cache_headers(),
                buf,
            ))
        }
        "fetch" => {
            if let Some(r) = refuse_fetch_while_draining("git-upload-pack") {
                return Ok(r);
            }
            let req = parse_fetch_request(reader).await?;
            // A want list far beyond any honest request is a blobless clone checking out HEAD without
            // `--sparse`/`--no-checkout` (git lazily asks for every blob of the tree in one fetch, with
            // `no-progress`, so nothing we narrate there is ever seen): refuse it with the fix before
            // any sync or pack work. `git.max_wants = 0` disables the bound.
            if st.cfg.git.max_wants > 0 && req.wants.len() > st.cfg.git.max_wants {
                let msg = too_many_wants_message(st, headers, route, req.wants.len());
                tracing::warn!(repo = %route.id, wants = req.wants.len(), max = st.cfg.git.max_wants, "fetch refused: too many wants");
                metrics::counter!("gitcask_fetch_too_many_wants_total", "repo" => route.id.to_string()).increment(1);
                return Ok(if req.sideband_all {
                    let mut buf = sideband_pkt(3, &msg);
                    pktline::encode_flush(&mut buf);
                    text_response(
                        "application/x-git-upload-pack-result",
                        no_cache_headers(),
                        buf,
                    )
                } else {
                    git_err_response("git-upload-pack", &msg)
                });
            }
            // Narrated fetch: the client accepted sideband-all and wants
            // progress → stream immediately and say what we are doing while
            // the local copy syncs, then hand over to upload-pack.
            if req.sideband_all && !req.no_progress {
                return Ok(narrated_fetch(st, route, headers, handle, req).await);
            }
            // Objects are needed from here on. Materialize every pack before
            // starting the streaming response so sync errors remain HTTP errors.
            drop(handle.sync_full().await?);
            let (writer, body) = write_body_pipe(256 * 1024);
            // Move the Arc<RepoHandle> into the spawned task so the ReadGuard
            // from sync_full() lives for the entire streaming response — packs must
            // not be removed mid-clone.
            let handle = handle.clone();
            tokio::spawn(async move {
                let guard = match handle.sync_full().await {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(error = ?e, "upload_pack v2 sync failed");
                        return;
                    }
                };
                // guard is held until the task ends (after streaming completes).
                if let Err(e) = run_fetch(&handle, req, writer).await {
                    tracing::warn!(error = ?e, "upload_pack v2 fetch failed");
                }
                drop(guard);
            });
            Ok(stream_response(
                "application/x-git-upload-pack-result",
                no_cache_headers(),
                body,
            ))
        }
        "object-info" => {
            let _guard = handle.sync_full().await?;
            let req = gitcask_git::pkt::parse_object_info(&cmd);
            let mut sizes_buf = Vec::with_capacity(256);
            let repo = handle.local().gix();
            for hex in &req.oids {
                let size = gix_hash::ObjectId::from_hex(hex.as_bytes())
                    .ok()
                    .and_then(|oid| repo.find_object(oid).ok())
                    .map(|o| o.data.len() as i64)
                    .unwrap_or(-1);
                pktline::encode_data(&mut sizes_buf, format!("size {size}\n").as_bytes());
            }
            pktline::encode_flush(&mut sizes_buf);
            Ok(text_response(
                "application/x-git-upload-pack-result",
                no_cache_headers(),
                sizes_buf,
            ))
        }
        other => Err(ApiError::BadRequest(format!("unknown v2 command: {other}"))),
    }
}

/// One sideband packet (`band` 1 = data, 2 = progress, 3 = error).
fn sideband_pkt(band: u8, text: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(text.len() + 8);
    let mut data = Vec::with_capacity(text.len() + 2);
    data.push(band);
    data.extend_from_slice(text.as_bytes());
    if !text.ends_with('\n') {
        data.push(b'\n');
    }
    for chunk in data.chunks(pktline::MAX_PKT_DATA) {
        pktline::encode_data(&mut buf, chunk);
    }
    buf
}

/// `git clone` shows band-2 lines as `remote: …`; prefix with `* ` so they read
/// as a narration of what the server is doing.
async fn say<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;
    let pkt = sideband_pkt(2, &format!("* {text}"));
    w.write_all(&pkt).await.is_ok() && w.flush().await.is_ok()
}

fn human(n: u64) -> String {
    bytesize::ByteSize::b(n).to_string()
}

/// Run a v2 fetch through stock git on the fully synced local copy.
async fn run_fetch<W: tokio::io::AsyncWrite + Unpin + Send>(
    handle: &Arc<gitcask_wal::RepoHandle>,
    req: gitcask_git::UploadPackRequest,
    writer: W,
) -> Result<(), gitcask_git::GitError> {
    let body = gitcask_git::build_v2_fetch_request(&req);
    handle
        .local()
        .upload_pack_raw(gitcask_git::pkt::Protocol::V2, &body[..], writer)
        .await
}

/// `handle.sync_full()` while narrating on band 2: the repo's progress packets
/// (notices, bars, task changes) as they happen, and a heartbeat every 5 s so
/// the connection never goes silent (a serverless host's frontend cut a push that
/// sent nothing for ~100 s while the broker materialized a large repository's side-files).
async fn sync_narrated<'h, W: tokio::io::AsyncWrite + Unpin>(
    handle: &'h Arc<gitcask_wal::RepoHandle>,
    writer: &mut W,
    t0: std::time::Instant,
) -> Result<gitcask_wal::ReadGuard<'h>, gitcask_wal::WalError> {
    let mut rx = handle.subscribe_progress();
    if !handle.packs_ready() {
        let _ = say(
            writer,
            "local copy is missing packs on this instance; materializing from the WAL…",
        )
        .await;
    }
    let sync = handle.sync_full();
    tokio::pin!(sync);
    let mut last_bar = std::time::Instant::now() - std::time::Duration::from_secs(1);
    loop {
        tokio::select! {
            biased;
            r = &mut sync => break r,
            p = rx.recv() => match p {
                Ok(gitcask_wal::Progress::Notice { text }) => { let _ = say(writer, &text).await; }
                Ok(gitcask_wal::Progress::Progress { label, done, total, unit, percent }) => {
                    if last_bar.elapsed() >= std::time::Duration::from_secs(1) || total.map(|t| done >= t).unwrap_or(false) {
                        last_bar = std::time::Instant::now();
                        let line = match (total, percent) {
                            (Some(t), Some(pc)) if unit == "bytes" => format!("{label}: {pc:.0}% ({} / {})", human(done), human(t)),
                            (Some(t), Some(pc)) => format!("{label}: {pc:.0}% ({done} / {t} {unit})"),
                            _ if unit == "bytes" => format!("{label}: {}", human(done)),
                            _ => format!("{label}: {done} {unit}"),
                        };
                        let _ = say(writer, &line).await;
                    }
                }
                Ok(gitcask_wal::Progress::Task { task }) => {
                    if task.ok.is_some() {
                        let _ = say(writer, &format!("task {} finished: {}", task.kind, task.summary)).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => {
                    // Channel closed: just await the sync.
                    break (&mut sync).await;
                }
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let _ = say(writer, &format!("still syncing ({}s)…", t0.elapsed().as_secs())).await;
            }
        }
    }
}

/// v2 `fetch` with narration: stream progress lines while the repo syncs
/// (materialize on a cold instance, WAL catch-up), then run `git upload-pack`
/// which continues in the same sideband framing. Errors after the stream has
/// started go out on band 3 (`remote error: …`).
async fn narrated_fetch(
    st: &AppState,
    route: &RepoRoute,
    headers: &HeaderMap,
    handle: &Arc<gitcask_wal::RepoHandle>,
    req: gitcask_git::UploadPackRequest,
) -> Response {
    let (mut writer, body) = write_body_pipe(256 * 1024);
    let handle = handle.clone();
    let repo = route.id.to_string();
    let who = st
        .auth
        .require_read(headers, route.id.owner(), route.id.name())
        .await
        .ok()
        .map(|p| p.name)
        .unwrap_or_else(|| "anonymous".into());
    let max_wants = st.cfg.git.max_wants;
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let t0 = std::time::Instant::now();
        if !say(
            &mut writer,
            &format!("gitcask: {repo} — authenticated as {who}"),
        )
        .await
        {
            return;
        }
        let applied = handle.applied_seq();
        let _ = say(
            &mut writer,
            &format!(
                "refs from the WAL at seq {applied}; you sent {} want(s), {} have(s){}",
                req.wants.len(),
                req.haves.len(),
                if req.haves.is_empty() {
                    " (full clone)"
                } else {
                    ""
                }
            ),
        )
        .await;
        // The initial fetch of a blobless clone is the one moment the user can still be told: the
        // lazy blob fetch that follows a checkout carries `no-progress`, so nothing said there is seen.
        if req.haves.is_empty()
            && req
                .filter
                .as_deref()
                .is_some_and(|f| f.starts_with("blob:none"))
            && req.deepen.is_none()
        {
            let _ = say(
                &mut writer,
                &format!(
                    "blobless clone: without --sparse or --no-checkout, checking out HEAD fetches every blob of its tree in \
                     one request next{}; `git sparse-checkout add <dir>` pulls only what you need",
                    if max_wants > 0 { format!(" (this host refuses requests above {max_wants} objects)") } else { String::new() }
                ),
            )
            .await;
        }
        // Sync with narration: forward the repo's progress packets while it runs.
        let guard = sync_narrated(&handle, &mut writer, t0).await;
        let guard = match guard {
            Ok(g) => g,
            Err(e) => {
                let msg = format!("gitcask: sync failed: {e}");
                // git dies on the first band-3 packet: send the whole
                // message (with its fix) in one.
                let _ = writer.write_all(&sideband_pkt(3, &msg)).await;
                let mut f = Vec::new();
                pktline::encode_flush(&mut f);
                let _ = writer.write_all(&f).await;
                let _ = writer.flush().await;
                return;
            }
        };
        let local = guard.local().clone();
        let packs = local.packs().map(|p| p.len()).unwrap_or(0);
        let _ = say(
            &mut writer,
            &format!(
                "local copy ready ({packs} pack(s), {:.1}s); computing what you are missing and packing it…",
                t0.elapsed().as_secs_f64()
            ),
        )
        .await;
        if let Err(e) = run_fetch(&handle, req, writer).await {
            tracing::warn!(error = ?e, "narrated upload_pack v2 fetch failed");
        }
        drop(guard);
    });
    stream_response(
        "application/x-git-upload-pack-result",
        no_cache_headers(),
        body,
    )
}

async fn upload_pack_v0(
    handle: &Arc<gitcask_wal::RepoHandle>,
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
) -> Result<Response, ApiError> {
    if let Some(r) = refuse_fetch_while_draining("git-upload-pack") {
        return Ok(r);
    }
    drop(handle.sync_full().await?);
    let (writer, body) = write_body_pipe(256 * 1024);
    // Move the Arc<RepoHandle> into the spawned task so the ReadGuard from
    // sync_full() lives for the entire streaming response.
    let handle = handle.clone();
    tokio::spawn(async move {
        let guard = match handle.sync_full().await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = ?e, "upload_pack v0 sync failed");
                return;
            }
        };
        let local = guard.local().clone();
        // guard held until task ends (after streaming completes).
        if let Err(e) = local
            .upload_pack_raw(gitcask_git::pkt::Protocol::V0, reader, writer)
            .await
        {
            tracing::warn!(error = ?e, "upload_pack v0 failed");
        }
        drop(guard);
    });
    Ok(stream_response(
        "application/x-git-upload-pack-result",
        no_cache_headers(),
        body,
    ))
}

/// Push URLs are `/<area>/<repository>.git` only (the `.git` suffix is required).
fn push_url_must_be_git(st: &AppState, route: &RepoRoute, headers: &HeaderMap) -> Option<String> {
    if route.had_git_suffix {
        return None;
    }
    Some(format!(
        "gitcask: push URL must be {}/<area>/<repository>.git",
        request_base_url(st, headers)
    ))
}

/// `POST /{owner}/{repo}.git/git-receive-pack`
pub async fn receive_pack(
    st: &Arc<AppState>,
    route: &RepoRoute,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let principal = st
        .auth
        .require_write(headers, route.id.owner(), route.id.name())
        .await?;
    if let Some(msg) = push_url_must_be_git(st, route, headers) {
        return refuse_push(body, headers, msg).await;
    }

    // Draining after SIGTERM: a push that starts now could not finish its
    // publish inside the grace; refuse it with Retry-After (git: rerun).
    if gitcask_wal::tasks::shutting_down() {
        metrics::counter!("gitcask_push_refused_total", "reason" => "draining").increment(1);
        return refuse_push(
            body,
            headers,
            "gitcask: this host is restarting; retry in a few seconds".into(),
        )
        .await
        .map(|r| with_retry_after(r, StatusCode::SERVICE_UNAVAILABLE));
    }
    let handle = open_repo(st, &route.id, true).await?;

    let enc = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let reader = maybe_gunzip(enc, body_to_async_read(body));

    // Parse commands + capabilities first (they need no objects); pack bytes
    // follow in `pack_reader`. Knowing the capabilities before the sync lets
    // us narrate the sync on band 2 when the client speaks side-band-64k.
    let (txn, caps, pack_reader) = gitcask_git::receive::parse(reader).await?;
    let pack_reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(pack_reader);
    // Wal's verify_txn treats empty string as the zero oid (create/delete).
    // receive::parse emits the 40-zero hex; normalize to empty for both ends.
    let mut txn = txn;
    for u in &mut txn.updates {
        if is_zero_oid(&u.old_oid) {
            u.old_oid.clear();
        }
        if is_zero_oid(&u.new_oid) {
            u.new_oid.clear();
        }
    }

    // Correlates the event with the user-visible request (docs/EVENTS.md).
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());

    if !caps.side_band_64k {
        // No sideband: the response is the report alone, after the work.
        let guard = handle.sync_full().await?;
        let report = receive_pack_process(
            st,
            &handle,
            guard,
            txn,
            caps,
            pack_reader,
            &principal,
            request_id,
        )
        .await?;
        return Ok(receive_response(report));
    }

    // Streaming: the report comes at the end; everything before it is band-2
    // narration (sync/materialize progress, heartbeat), so the connection is
    // never silent while this host brings a big repository's side-files in.
    let (mut writer, body) = write_body_pipe(64 * 1024);
    let handle = handle.clone();
    let st_arc = st.clone();
    let repo = route.id.to_string();
    let who = principal.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let t0 = std::time::Instant::now();
        let _ = say(
            &mut writer,
            &format!("gitcask: {repo} — push by {}", who.name),
        )
        .await;
        let guard = match sync_narrated(&handle, &mut writer, t0).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "receive-pack: sync failed");
                let report =
                    refusal_report(&caps, &txn, &format!("gitcask: sync failed: {e}")).await;
                let _ = writer.write_all(&report).await;
                let _ = writer.flush().await;
                return;
            }
        };
        if t0.elapsed().as_secs() >= 2 {
            let _ = say(
                &mut writer,
                &format!(
                    "local copy ready ({:.1}s); unpacking and checking your objects…",
                    t0.elapsed().as_secs_f64()
                ),
            )
            .await;
        }
        let txn_for_report = gitcask_proto::v1::RefTransaction {
            updates: txn.updates.clone(),
            ..Default::default()
        };
        let report = match receive_pack_process(
            &st_arc,
            &handle,
            guard,
            txn,
            caps.clone(),
            pack_reader,
            &who,
            request_id,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e.message(), "receive-pack failed");
                let message = if e.status() == StatusCode::SERVICE_UNAVAILABLE {
                    "gitcask: transient storage failure; retry this push".to_string()
                } else {
                    format!("gitcask: {}", e.message())
                };
                refusal_report(&caps, &txn_for_report, &message).await
            }
        };
        let _ = writer.write_all(&report).await;
        let _ = writer.flush().await;
    });
    Ok(stream_response(
        "application/x-git-receive-pack-result",
        no_cache_headers(),
        body,
    ))
}

/// Everything after the sync: unpack, connectivity, publish → the
/// report-status bytes (already sideband-framed when the client asked).
async fn receive_pack_process(
    st: &AppState,
    handle: &Arc<gitcask_wal::RepoHandle>,
    _guard: gitcask_wal::ReadGuard<'_>,
    mut txn: gitcask_proto::v1::RefTransaction,
    caps: gitcask_git::receive::ReceiveCaps,
    pack_reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    principal: &crate::auth::Principal,
    request_id: Option<String>,
) -> Result<Vec<u8>, ApiError> {
    let route_id = handle.id().clone();
    let max_bytes = Some(st.cfg.server.max_push_bytes.as_u64());
    let opts = gitcask_git::IngestOptions {
        fsck: st.cfg.wal.fsck_objects,
        max_bytes,
        thin: true,
    };
    let local = handle.local().clone();
    let ingest = local
        .ingest_pack(pack_reader, opts)
        .instrument(tracing::info_span!("receive.ingest"))
        .await;

    let unpack_err: Option<String> = match &ingest {
        Ok(_) => None,
        Err(e) => Some(format!("unpack failed: {e}")),
    };

    // Connectivity check for pushed tips (before we publish anything).
    if unpack_err.is_none() && st.cfg.wal.check_connectivity {
        if let Ok(Some(_)) = &ingest {
            let tips: Vec<gix_hash::ObjectId> = txn
                .updates
                .iter()
                .filter(|u| !u.new_oid.is_empty() && !is_zero_oid(&u.new_oid))
                .filter_map(|u| gix_hash::ObjectId::from_hex(u.new_oid.as_bytes()).ok())
                .collect();
            if !tips.is_empty() {
                if let Err(e) = local
                    .check_connectivity_async(&tips, true)
                    .instrument(tracing::info_span!(
                        "receive.connectivity",
                        tips = tips.len()
                    ))
                    .await
                {
                    // Every refusal names the reason on each ref: `unpack ng`
                    // alone makes git print "remote failed to report status".
                    tracing::warn!(repo = %route_id, error = %e, "receive-pack: connectivity check failed");
                    metrics::counter!("gitcask_push_refused_total", "reason" => "connectivity")
                        .increment(1);
                    return Ok(refusal_report(&caps, &txn, &format!("connectivity: {e}")).await);
                }
            }
        }
    }

    // On unpack failure, report and abort (nothing was published).
    if let Some(msg) = unpack_err {
        tracing::warn!(repo = %route_id, error = %msg, "receive-pack: unpack failed");
        metrics::counter!("gitcask_push_refused_total", "reason" => "unpack").increment(1);
        return Ok(refusal_report(&caps, &txn, &msg).await);
    }

    let unpack_result: Result<(), String> = Ok(());

    // Release the sync read guard before publishing. `publish_push_synced`
    // reuses this request's freshness check while still syncing after CAS
    // conflicts.
    drop(_guard);

    // Writer-side peel: replicas advertise annotated tags without objects.
    local.fill_peeled(&mut txn);
    let meta = push_meta(&caps, principal, &txn, &request_id);
    let pack_ref = match ingest {
        Ok(Some(p)) => Some(p),
        _ => None,
    };
    let publish = handle
        .publish_push_synced(pack_ref, txn, meta)
        .instrument(tracing::info_span!("receive.publish"))
        .await;
    let (seq, per_ref): (u64, Vec<(String, Result<(), String>)>) = match publish {
        Ok(r) => (
            r.seq,
            r.per_ref
                .into_iter()
                .map(|(n, r)| (n, r.map_err(|e| e.to_string())))
                .collect(),
        ),
        Err(e) => {
            tracing::error!(error = ?e, "receive-pack publish failed");
            return Err(e.into());
        }
    };
    tracing::info!(seq, refs = per_ref.len(), "receive-pack published");

    let report = build_report(&caps, unpack_result, &per_ref).await;
    Ok(report)
}

async fn build_report(
    caps: &gitcask_git::receive::ReceiveCaps,
    unpack: Result<(), String>,
    per_ref: &[(String, Result<(), String>)],
) -> Vec<u8> {
    let mut w = VecWriter::new();
    let _ = gitcask_git::receive::report_status(caps, unpack, per_ref, &mut w).await;
    w.into_inner()
}

/// The report for a push refused before any work: `unpack ng <msg>`, `ng` on
/// every ref; with side-band the message goes out on band 2 first (`remote:
/// …`). Not band 3: git treats it as a fatal transport error ("the remote end
/// hung up unexpectedly") and never shows the per-ref `[remote rejected]`.
async fn refusal_report(
    caps: &gitcask_git::receive::ReceiveCaps,
    txn: &gitcask_proto::v1::RefTransaction,
    msg: &str,
) -> Vec<u8> {
    let per_ref: Vec<(String, Result<(), String>)> = txn
        .updates
        .iter()
        .map(|u| (u.name.clone(), Err(msg.to_string())))
        .collect();
    let mut out = Vec::new();
    if caps.side_band_64k {
        out.extend_from_slice(&sideband_pkt(2, &format!("{msg}\n")));
    }
    out.extend(build_report(caps, Err(msg.to_string()), &per_ref).await);
    out
}

/// Refuse a push whose commands have not been parsed yet: read the command
/// section only (no pack bytes), answer the refusal, drop the rest of the body.
async fn refuse_push(body: Body, headers: &HeaderMap, msg: String) -> Result<Response, ApiError> {
    let enc = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok());
    let reader = maybe_gunzip(enc, body_to_async_read(body));
    let (txn, caps, _pack) = gitcask_git::receive::parse(reader).await?;
    Ok(receive_response(refusal_report(&caps, &txn, &msg).await))
}

/// Turn a refusal into a 503 the edge and scripts can act on (`Retry-After`),
/// keeping the git report body so git itself still prints the reason.
fn with_retry_after(mut resp: Response, status: StatusCode) -> Response {
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("15"),
    );
    resp
}

fn receive_response(report: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, report).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/x-git-receive-pack-result".parse().unwrap(),
    );
    resp
}

fn push_meta(
    caps: &gitcask_git::receive::ReceiveCaps,
    principal: &crate::auth::Principal,
    txn: &gitcask_proto::v1::RefTransaction,
    request_id: &Option<String>,
) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("agent".to_string(), caps.agent.clone().unwrap_or_default());
    m.insert("principal".to_string(), principal.name.clone());
    m.insert("push_options".to_string(), txn.push_options.join("\n"));
    if let Some(rid) = request_id {
        m.insert("request_id".to_string(), rid.clone());
    }
    m
}

fn is_zero_oid(hex: &str) -> bool {
    hex.chars().all(|c| c == '0') && !hex.is_empty()
}

/// Parse the v2 `fetch` command body (want/have/done/...) into [`UploadPackRequest`].
/// Reads pkt-lines until a flush (stateless) or `done` + flush.
async fn parse_fetch_request(
    mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
) -> Result<gitcask_git::UploadPackRequest, ApiError> {
    let mut req = gitcask_git::UploadPackRequest {
        wants: Vec::new(),
        haves: Vec::new(),
        done: false,
        thin_pack: false,
        no_progress: false,
        include_tag: false,
        ofs_delta: false,
        sideband_all: false,
        wait_for_done: false,
        filter: None,
        deepen: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        shallow: Vec::new(),
        want_refs: Vec::new(),
    };
    loop {
        let line = gitcask_git::pkt::read_pkt_line(&mut reader).await?;
        match line {
            None
            | Some(gitcask_git::pkt::PktLine::Flush)
            | Some(gitcask_git::pkt::PktLine::Delim) => break,
            Some(gitcask_git::pkt::PktLine::ResponseEnd) => break,
            Some(gitcask_git::pkt::PktLine::Data(b)) => {
                let s = String::from_utf8_lossy(&b);
                let s = s.trim_end_matches('\n');
                if let Some(hex) = s.strip_prefix("want ") {
                    if let Ok(oid) = gix_hash::ObjectId::from_hex(hex.as_bytes()) {
                        req.wants.push(oid);
                    }
                } else if let Some(hex) = s.strip_prefix("have ") {
                    if let Ok(oid) = gix_hash::ObjectId::from_hex(hex.as_bytes()) {
                        req.haves.push(oid);
                    }
                } else if s == "done" {
                    req.done = true;
                } else if s == "thin-pack" {
                    req.thin_pack = true;
                } else if s == "no-progress" {
                    req.no_progress = true;
                } else if s == "include-tag" {
                    req.include_tag = true;
                } else if s == "ofs-delta" {
                    req.ofs_delta = true;
                } else if s == "sideband-all" {
                    req.sideband_all = true;
                } else if s == "wait-for-done" {
                    req.wait_for_done = true;
                } else if let Some(spec) = s.strip_prefix("filter ") {
                    req.filter = Some(spec.to_string());
                } else if let Some(n) = s.strip_prefix("deepen ") {
                    req.deepen = n.parse().ok();
                } else if let Some(t) = s.strip_prefix("deepen-since ") {
                    req.deepen_since = t.parse().ok();
                } else if let Some(r) = s.strip_prefix("deepen-not ") {
                    req.deepen_not.push(r.to_string());
                } else if let Some(hex) = s.strip_prefix("shallow ") {
                    if let Ok(oid) = gix_hash::ObjectId::from_hex(hex.as_bytes()) {
                        req.shallow.push(oid);
                    }
                } else if let Some(r) = s.strip_prefix("want-ref ") {
                    req.want_refs.push(r.to_string());
                }
            }
        }
    }
    Ok(req)
}

// ---- helpers ----

pub(crate) async fn open_repo(
    st: &AppState,
    id: &gitcask_git::RepoId,
    write: bool,
) -> Result<Arc<gitcask_wal::RepoHandle>, ApiError> {
    if write && st.cfg.server.auto_create_on_push {
        let format = gitcask_git::ObjectFormat::from(st.cfg.git.object_format);
        Ok(st.registry.open_or_create(id, format).await?)
    } else {
        Ok(st.registry.open(id).await?)
    }
}

fn text_response(
    ct: &str,
    headers: [(axum::http::HeaderName, &'static str); 3],
    body: Vec<u8>,
) -> Response {
    build_response(StatusCode::OK, ct, headers, body)
}

fn stream_response(
    ct: &str,
    headers: [(axum::http::HeaderName, &'static str); 3],
    body: Body,
) -> Response {
    build_response(StatusCode::OK, ct, headers, body)
}

pub(crate) fn build_response<B: axum::response::IntoResponse>(
    status: StatusCode,
    ct: &str,
    extra: [(axum::http::HeaderName, &'static str); 3],
    body: B,
) -> Response {
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(axum::http::header::CONTENT_TYPE, ct.parse().unwrap());
    for (k, v) in extra {
        h.insert(k, v.parse().unwrap());
    }
    resp
}

/// Public base URL (`scheme://host`, no trailing slash) for links we hand to
/// clients: `server.public_url` if configured, else reconstructed from the
/// request (`X-Forwarded-Proto`/`X-Forwarded-Host`/`Host`), else the listen port.
pub(crate) fn request_base_url(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(u) = &st.cfg.server.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    match host {
        Some(h) => {
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
                .filter(|s| s == "http" || s == "https")
                .unwrap_or_else(|| {
                    if h.starts_with("localhost") || h.starts_with("127.") || h.starts_with("[::1]")
                    {
                        "http".into()
                    } else {
                        "https".into()
                    }
                });
            format!("{scheme}://{h}")
        }
        None => crate::listen_url(&st.cfg),
    }
}

/// New fetches are refused while the process drains, before any sync starts.
fn refuse_fetch_while_draining(service: &str) -> Option<Response> {
    if gitcask_wal::tasks::shutting_down() {
        return Some(with_retry_after(
            git_err_response(
                service,
                "gitcask: this host is restarting; retry in a few seconds",
            ),
            StatusCode::SERVICE_UNAVAILABLE,
        ));
    }
    None
}

/// A 200 response carrying a pkt-line `ERR <msg>` so git prints it verbatim.
/// The refusal for a fetch whose want list exceeds `git.max_wants`, with the fix.
fn too_many_wants_message(
    st: &AppState,
    headers: &HeaderMap,
    route: &RepoRoute,
    wants: usize,
) -> String {
    let base = request_base_url(st, headers);
    let repo = &route.id;
    format!(
        "gitcask: this fetch asks for {wants} objects at once (this host's bound is {max}). That is what a \
         `git clone --filter=blob:none` does right after cloning when it checks out HEAD: every blob of the tree \
         in one request. Clone blobless with a sparse or no checkout instead, then fetch blobs as you need them:\n  \
         git clone --filter=blob:none --sparse {base}/{repo}.git\n  \
         git sparse-checkout add <dir>…\nor, for the whole tree: \
         git clone {base}/{repo}.git",
        max = st.cfg.git.max_wants,
    )
}

fn git_err_response(service: &str, msg: &str) -> Response {
    let mut buf = Vec::with_capacity(msg.len() + 64);
    pktline::encode_data(&mut buf, format!("ERR {msg}\n").as_bytes());
    pktline::encode_flush(&mut buf);
    text_response(
        match service {
            "git-receive-pack" => "application/x-git-receive-pack-advertisement",
            _ => "application/x-git-upload-pack-advertisement",
        },
        no_cache_headers(),
        buf,
    )
}
