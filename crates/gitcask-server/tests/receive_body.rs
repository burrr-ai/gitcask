//! When receive-pack's response starts (D52).
//!
//! A proxy that forwards the request body over HTTP/1 may stop forwarding it
//! once the upstream answers: in production (AWS ALB → HTTP/1 target) a
//! side-band-64k push whose response headers and band-2 banner went out right
//! after the ref commands stalled forever in the pack body read. Over HTTP/1.x
//! gitcask now answers only after the request body ends, replaying the sync
//! narration it buffered meanwhile; over HTTP/2 a side-band push narrates from
//! the start. Most tests drive a raw HTTP/1.1 connection so the body can be
//! paced and the first response byte timed against the last request byte; the
//! HTTP/2 tests use an h2c (prior knowledge) client.
mod harness;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bytesize::ByteSize;
use harness::{Server, TestRepo, git, git_in};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `git pack-objects --stdout --revs` of everything reachable from `main`.
fn pack_of_main(repo: &Path) -> Result<Vec<u8>> {
    pack_objects(repo, "refs/heads/main\n")
}

/// `git pack-objects --stdout --revs --thin` for `revs` (one rev per line).
fn pack_objects(repo: &Path, revs: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "--thin", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .context("pack-objects stdin")?
            .write_all(revs.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

/// A receive-pack request creating `refs/heads/main` at `head`, with `pack`.
fn push_request(head: &str, caps: &str, pack: &[u8]) -> Vec<u8> {
    update_request(&"0".repeat(40), head, caps, pack)
}

/// A receive-pack request moving `refs/heads/main` from `old` to `new`.
fn update_request(old: &str, new: &str, caps: &str, pack: &[u8]) -> Vec<u8> {
    let cmd = format!("{old} {new} refs/heads/main\0{caps}\n");
    let mut body = format!("{:04x}{cmd}0000", cmd.len() + 4).into_bytes();
    body.extend_from_slice(pack);
    body
}

async fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    async_compression::tokio::bufread::GzipEncoder::new(bytes)
        .read_to_end(&mut out)
        .await?;
    Ok(out)
}

/// Decode an HTTP/1.1 response with a chunked or content-length body.
fn parse_response(raw: &[u8]) -> Result<(String, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no end of response headers")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut rest = &raw[split + 4..];
    if !head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Ok((head, rest.to_vec()));
    }
    let mut body = Vec::new();
    loop {
        let eol = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("chunk size line")?;
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..eol])?.trim(), 16)?;
        rest = &rest[eol + 2..];
        if size == 0 {
            break;
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    Ok((head, body))
}

/// Split a side-band response body into `(band, payload)` packets, in order.
fn sideband_packets(body: &[u8]) -> Result<Vec<(u8, String)>> {
    let mut out = Vec::new();
    let mut rest = body;
    while rest.len() >= 4 {
        let len = usize::from_str_radix(std::str::from_utf8(&rest[..4])?, 16)?;
        if len == 0 {
            rest = &rest[4..];
            continue;
        }
        ensure!(len >= 5 && len <= rest.len(), "bad pkt-line length {len}");
        out.push((rest[4], String::from_utf8_lossy(&rest[5..len]).to_string()));
        rest = &rest[len..];
    }
    ensure!(rest.is_empty(), "trailing bytes after the last pkt-line");
    Ok(out)
}

/// A memory store whose every operation takes `latency`, so a sync is slow.
fn slow_store(latency: Duration) -> std::sync::Arc<gitcask_store::memory::MemoryStore> {
    let mut store = gitcask_store::memory::MemoryStore::new();
    store.latency = Some(latency);
    std::sync::Arc::new(store)
}

/// A second commit on `main` in `src`; returns (old, new) tips.
fn advance_main(src: &Path) -> Result<(String, String)> {
    let old = git_in(src, &["rev-parse", "main"])?.trim().to_string();
    git(&["checkout", "-q", "-f", "main"], src)?;
    std::fs::write(src.join("next.txt"), "next revision\n")?;
    git(&["add", "next.txt"], src)?;
    git(&["commit", "-q", "-m", "next"], src)?;
    let new = git_in(src, &["rev-parse", "main"])?.trim().to_string();
    Ok((old, new))
}

/// Walk `dir` and return every file whose name marks an in-flight or leaked
/// pack reception (anything but a published `pack-<checksum>.*`). The sync's
/// download directory `.gitcask-tmp` may exist but must be empty.
fn stray_pack_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let name = ent.file_name().to_string_lossy().to_string();
            if name == ".gitcask-tmp" && path.is_dir() {
                out.extend(
                    std::fs::read_dir(&path)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path().display().to_string()),
                );
            } else if name.starts_with("gitcask-ingest-")
                || name.contains("tmp")
                || (name.ends_with(".pack") && !name.starts_with("pack-"))
            {
                out.push(path.display().to_string());
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out
}

/// POST `body` to receive-pack as HTTP/1.1 chunked, `pieces` chunks paced by
/// `gap`, the terminating chunk after one more `gap`. Returns the raw response,
/// the instant the terminator was written, and the instant the first response
/// byte arrived.
async fn paced_post(
    server: &Server,
    path: &str,
    body: &[u8],
    gzip_encoded: bool,
    pieces: usize,
    gap: Duration,
) -> Result<(Vec<u8>, Instant, Instant)> {
    let addr = server.base_url.trim_start_matches("http://").to_string();
    let stream = tokio::net::TcpStream::connect(&addr).await?;
    let (mut rd, mut wr) = stream.into_split();
    let reader = tokio::spawn(async move {
        let mut raw = Vec::new();
        let mut first = None;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = rd.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            first.get_or_insert_with(Instant::now);
            raw.extend_from_slice(&buf[..n]);
        }
        anyhow::Ok((raw, first))
    });
    let encoding = if gzip_encoded {
        "Content-Encoding: gzip\r\n"
    } else {
        ""
    };
    wr.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/x-git-receive-pack-request\r\n\
             Accept: application/x-git-receive-pack-result\r\n\
             {encoding}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await?;
    let piece = body.len().div_ceil(pieces);
    for chunk in body.chunks(piece) {
        wr.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        wr.write_all(chunk).await?;
        wr.write_all(b"\r\n").await?;
        wr.flush().await?;
        tokio::time::sleep(gap).await;
    }
    let body_end = Instant::now();
    wr.write_all(b"0\r\n\r\n").await?;
    wr.flush().await?;
    let (raw, first) = tokio::time::timeout(Duration::from_secs(60), reader)
        .await
        .context("response did not finish")???;
    let first = first.context("empty response")?;
    Ok((raw, body_end, first))
}

async fn slow_sideband_push(gzip_encoded: bool) -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "slow").await?;
    let src = TestRepo::synthetic(20, 4)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let pack = pack_of_main(&src)?;
    let mut body = push_request(&head, "report-status side-band-64k agent=git/2.50.0", &pack);
    if gzip_encoded {
        body = gzip(&body).await?;
    }

    let (raw, body_end, first_byte) = paced_post(
        &server,
        "/t/slow.git/git-receive-pack",
        &body,
        gzip_encoded,
        8,
        Duration::from_millis(150),
    )
    .await?;
    // Status line, headers and the band-2 banner all wait for the terminator.
    ensure!(
        first_byte >= body_end,
        "gzip={gzip_encoded}: response started {:?} before the request body ended",
        body_end - first_byte
    );
    let (head_lines, report) = parse_response(&raw)?;
    ensure!(
        head_lines.starts_with("HTTP/1.1 200"),
        "gzip={gzip_encoded}: {head_lines}"
    );
    let report = String::from_utf8_lossy(&report).to_string();
    ensure!(report.contains("gitcask: t/slow"), "banner: {report}");
    ensure!(report.contains("unpack ok"), "{report}");
    ensure!(report.contains("ok refs/heads/main"), "{report}");

    let refs = server.ls_remote("t", "slow").await?;
    ensure!(
        refs.contains(&format!("{head}\trefs/heads/main")),
        "gzip={gzip_encoded}: {refs}"
    );
    let clone = tempfile::tempdir()?;
    git(
        &["clone", "-q", &server.repo_url("t", "slow"), "."],
        clone.path(),
    )?;
    git(&["fsck", "--full"], clone.path())?;
    ensure!(git_in(clone.path(), &["rev-parse", "HEAD"])?.trim() == head);
    ensure!(
        git_in(clone.path(), &["rev-parse", "HEAD^{tree}"])?
            == git_in(&src, &["rev-parse", "main^{tree}"])?
    );
    let strays = stray_pack_files(&server.state.cfg.cache.dir);
    ensure!(strays.is_empty(), "leftover files: {strays:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sideband_push_answers_only_after_the_request_body_ends() -> Result<()> {
    slow_sideband_push(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gzip_sideband_push_answers_only_after_the_request_body_ends() -> Result<()> {
    slow_sideband_push(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_push_is_refused_and_leaves_no_files() -> Result<()> {
    let limit = 1024;
    let server = Server::start_with_tweak(|c| c.server.max_push_bytes = ByteSize::b(limit)).await?;
    server.put_repo("t", "big").await?;
    let src = TestRepo::synthetic(40, 4)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let pack = pack_of_main(&src)?;
    ensure!(
        pack.len() as u64 > limit,
        "pack is only {} bytes",
        pack.len()
    );

    for caps in [
        "report-status side-band-64k agent=git/2.50.0",
        "report-status agent=git/2.50.0",
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{}/t/big.git/git-receive-pack", server.base_url))
            .header("Content-Type", "application/x-git-receive-pack-request")
            .body(push_request(&head, caps, &pack))
            .send()
            .await?;
        ensure!(resp.status() == reqwest::StatusCode::OK, "{caps}");
        let report = resp.text().await?;
        ensure!(report.contains("unpack "), "{caps}: {report}");
        ensure!(!report.contains("unpack ok"), "{caps}: {report}");
        ensure!(
            report.contains("ng refs/heads/main") && report.contains("pack exceeds max_bytes 1024"),
            "{caps}: {report}"
        );
    }
    let refs = server.ls_remote("t", "big").await?;
    ensure!(!refs.contains("refs/heads/main"), "{refs}");
    let strays = stray_pack_files(&server.state.cfg.cache.dir);
    ensure!(strays.is_empty(), "leftover files: {strays:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_upload_publishes_nothing_and_leaves_no_files() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("t", "gone").await?;
    let src = TestRepo::synthetic(20, 4)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let pack = pack_of_main(&src)?;
    let body = push_request(&head, "report-status side-band-64k", &pack);

    let addr = server.base_url.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    stream
        .write_all(
            format!(
                "POST /t/gone.git/git-receive-pack HTTP/1.1\r\nHost: {addr}\r\n\
                 Content-Type: application/x-git-receive-pack-request\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(&body[..body.len() / 2]).await?;
    stream.flush().await?;
    // Nothing is answered while the body is incomplete.
    let mut buf = [0u8; 1];
    ensure!(
        tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf))
            .await
            .is_err(),
        "response started before the request body ended"
    );
    drop(stream);

    // The server notices the disconnect and drops the reception.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let strays = stray_pack_files(&server.state.cfg.cache.dir);
        if strays.is_empty() {
            break;
        }
        ensure!(Instant::now() < deadline, "leftover files: {strays:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let refs = server.ls_remote("t", "gone").await?;
    ensure!(!refs.contains("refs/heads/main"), "{refs}");
    ensure!(server.read_log("t", "gone").await?.is_empty());
    Ok(())
}

/// A push to an instance that must first materialize the repository: the sync
/// runs while the pack uploads, its narration is held back, and after the body
/// ends the response replays it in order: banner, sync narration, report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_instance_replays_sync_narration_after_the_body_ends() -> Result<()> {
    cold_instance_push(true).await
}

/// Without side-band there is nothing to narrate, but the sync still overlaps
/// the upload and the one report follows the body's end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_instance_plain_push_overlaps_the_sync() -> Result<()> {
    cold_instance_push(false).await
}

async fn cold_instance_push(sideband: bool) -> Result<()> {
    let caps = if sideband {
        "report-status side-band-64k"
    } else {
        "report-status"
    };
    let warm =
        Server::start_with_store_and_tweak(slow_store(Duration::from_millis(40)), |_| {}).await?;
    warm.put_repo("t", "cold").await?;
    let src = TestRepo::synthetic(10, 3)?;
    git_in(&src, &["push", "-q", &warm.repo_url("t", "cold"), "main"])?;
    let (old, new) = advance_main(&src)?;
    let pack = pack_objects(&src, &format!("{new}\n^{old}\n"))?;
    let body = update_request(&old, &new, caps, &pack);

    let cold = warm
        .start_sibling_with(|c| c.wal.prefetch_packs = false)
        .await?;
    // The sync overlaps the upload: this instance's copy is ready before the
    // request body ends.
    let handle = cold
        .state
        .registry
        .open(&gitcask_git::RepoId::new("t", "cold")?)
        .await?;
    ensure!(!handle.packs_ready(), "the sibling already has the packs");
    let ready = tokio::spawn(async move {
        while !handle.packs_ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Instant::now()
    });
    let (raw, body_end, first_byte) = paced_post(
        &cold,
        "/t/cold.git/git-receive-pack",
        &body,
        false,
        8,
        Duration::from_millis(150),
    )
    .await?;
    ensure!(
        first_byte >= body_end,
        "response started {:?} before the request body ended",
        body_end - first_byte
    );
    let ready_at = tokio::time::timeout(Duration::from_secs(10), ready).await??;
    ensure!(
        ready_at < body_end,
        "the sync finished {:?} after the request body ended; it did not overlap the upload",
        ready_at - body_end
    );
    let (head, body) = parse_response(&raw)?;
    ensure!(head.starts_with("HTTP/1.1 200"), "{head}");
    if sideband {
        let packets = sideband_packets(&body)?;
        let find = |band: u8, needle: &str| {
            packets
                .iter()
                .position(|(b, text)| *b == band && text.contains(needle))
        };
        let banner = find(2, "gitcask: t/cold — push by").context("no banner")?;
        let replayed = find(2, "materializing from the WAL").context("no sync narration")?;
        let report = find(1, "unpack ok").context("no unpack ok")?;
        ensure!(
            banner == 0 && banner < replayed && replayed < report,
            "order banner={banner} replayed={replayed} report={report}: {packets:?}"
        );
        ensure!(find(1, "ok refs/heads/main").is_some(), "{packets:?}");
    } else {
        let report = String::from_utf8_lossy(&body).to_string();
        ensure!(
            report.contains("unpack ok") && report.contains("ok refs/heads/main"),
            "{report}"
        );
    }

    let clone = tempfile::tempdir()?;
    git(
        &["clone", "-q", &cold.repo_url("t", "cold"), "."],
        clone.path(),
    )?;
    git(&["fsck", "--full"], clone.path())?;
    ensure!(git_in(clone.path(), &["rev-parse", "HEAD"])?.trim() == new);
    let strays = stray_pack_files(&cold.state.cfg.cache.dir);
    ensure!(strays.is_empty(), "leftover files: {strays:?}");
    Ok(())
}

/// An upload abandoned while the overlapping sync is still materializing: the
/// sync is cancelled with its read guard, nothing is published or left on disk,
/// and the next push to the same repository on that instance succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_upload_during_sync_releases_the_repository() -> Result<()> {
    let warm =
        Server::start_with_store_and_tweak(slow_store(Duration::from_millis(250)), |_| {}).await?;
    warm.put_repo("t", "busy").await?;
    let src = TestRepo::synthetic(10, 3)?;
    git_in(&src, &["push", "-q", &warm.repo_url("t", "busy"), "main"])?;
    let (old, new) = advance_main(&src)?;
    let pack = pack_objects(&src, &format!("{new}\n^{old}\n"))?;
    let body = update_request(&old, &new, "report-status side-band-64k", &pack);

    let cold = warm
        .start_sibling_with(|c| c.wal.prefetch_packs = false)
        .await?;
    let addr = cold.base_url.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(&addr).await?;
    stream
        .write_all(
            format!(
                "POST /t/busy.git/git-receive-pack HTTP/1.1\r\nHost: {addr}\r\n\
                 Content-Type: application/x-git-receive-pack-request\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(&body[..body.len() / 2]).await?;
    stream.flush().await?;
    let mut buf = [0u8; 1];
    ensure!(
        tokio::time::timeout(Duration::from_millis(400), stream.read(&mut buf))
            .await
            .is_err(),
        "response started before the request body ended"
    );
    let id = gitcask_git::RepoId::new("t", "busy")?;
    ensure!(
        !cold.state.registry.open(&id).await?.packs_ready(),
        "the sync finished before the upload was abandoned; nothing was in flight"
    );
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let strays = stray_pack_files(&cold.state.cfg.cache.dir);
        if strays.is_empty() {
            break;
        }
        ensure!(Instant::now() < deadline, "leftover files: {strays:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(cold.read_log("t", "busy").await?.len() == 1);

    let (raw, body_end, first_byte) = paced_post(
        &cold,
        "/t/busy.git/git-receive-pack",
        &body,
        false,
        2,
        Duration::from_millis(50),
    )
    .await?;
    ensure!(first_byte >= body_end);
    let (_, report) = parse_response(&raw)?;
    let report = String::from_utf8_lossy(&report).to_string();
    ensure!(
        report.contains("unpack ok") && report.contains("ok refs/heads/main"),
        "{report}"
    );
    let refs = cold.ls_remote("t", "busy").await?;
    ensure!(refs.contains(&format!("{new}\trefs/heads/main")), "{refs}");
    let strays = stray_pack_files(&cold.state.cfg.cache.dir);
    ensure!(strays.is_empty(), "leftover files: {strays:?}");
    Ok(())
}

/// Over HTTP/2 (h2c, prior knowledge) a stream is full-duplex, so a side-band
/// push narrates from the start: the status and the banner arrive while the
/// body is still uploading, and the push publishes. A push without side-band
/// still answers only after the body ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http2_sideband_push_narrates_before_the_body_ends() -> Result<()> {
    h2_push(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http2_plain_push_answers_only_after_the_body_ends() -> Result<()> {
    h2_push(false).await
}

async fn h2_push(sideband: bool) -> Result<()> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, StreamBody};
    use hyper::body::Frame;

    let server = Server::start().await?;
    server.put_repo("t", "h2").await?;
    let src = TestRepo::synthetic(20, 4)?;
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let caps = if sideband {
        "report-status side-band-64k"
    } else {
        "report-status"
    };
    let body = push_request(&head, caps, &pack_of_main(&src)?);

    let addr = server.base_url.trim_start_matches("http://").to_string();
    let tcp = tokio::net::TcpStream::connect(&addr).await?;
    let (mut sender, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(tcp),
    )
    .await?;
    tokio::spawn(conn);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::convert::Infallible>>(8);
    let request = http::Request::post(format!("http://{addr}/t/h2.git/git-receive-pack"))
        .header("content-type", "application/x-git-receive-pack-request")
        .body(StreamBody::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))?;
    let response = tokio::spawn(sender.send_request(request));

    // Commands and the first half of the pack; the rest is held back.
    let (first, rest) = body.split_at(body.len() / 2);
    tx.send(Ok(Frame::data(Bytes::copy_from_slice(first))))
        .await
        .ok()
        .context("request body closed")?;
    let mut response = response;
    let mut early = None;
    if sideband {
        let resp = tokio::time::timeout(Duration::from_secs(10), &mut response)
            .await
            .context("no HTTP/2 response before the request body ended")???;
        ensure!(resp.status() == 200, "{}", resp.status());
        let mut incoming = resp.into_body();
        let banner = tokio::time::timeout(Duration::from_secs(10), incoming.frame())
            .await
            .context("no banner before the request body ended")?
            .context("response ended")??
            .into_data()
            .ok()
            .context("not a data frame")?;
        ensure!(
            String::from_utf8_lossy(&banner).contains("gitcask: t/h2 — push by"),
            "{banner:?}"
        );
        early = Some((banner, incoming));
    } else {
        ensure!(
            tokio::time::timeout(Duration::from_millis(500), &mut response)
                .await
                .is_err(),
            "a push without side-band answered before the request body ended"
        );
    }
    for chunk in rest.chunks(rest.len().div_ceil(4)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(Ok(Frame::data(Bytes::copy_from_slice(chunk))))
            .await
            .ok()
            .context("request body closed")?;
    }
    drop(tx);

    let (mut raw, incoming) = match early {
        Some((banner, incoming)) => (banner.to_vec(), incoming),
        None => {
            let resp = tokio::time::timeout(Duration::from_secs(30), response)
                .await
                .context("no response")???;
            ensure!(resp.status() == 200, "{}", resp.status());
            (Vec::new(), resp.into_body())
        }
    };
    let rest = tokio::time::timeout(Duration::from_secs(30), incoming.collect())
        .await
        .context("response did not finish")??
        .to_bytes();
    raw.extend_from_slice(&rest);
    let report = if sideband {
        let packets = sideband_packets(&raw)?;
        ensure!(
            packets[0].0 == 2 && packets[0].1.contains("push by"),
            "{packets:?}"
        );
        packets
            .into_iter()
            .filter(|(band, _)| *band == 1)
            .map(|(_, text)| text)
            .collect::<String>()
    } else {
        String::from_utf8_lossy(&raw).to_string()
    };
    ensure!(
        report.contains("unpack ok") && report.contains("ok refs/heads/main"),
        "{report}"
    );
    let refs = server.ls_remote("t", "h2").await?;
    ensure!(refs.contains(&format!("{head}\trefs/heads/main")), "{refs}");
    Ok(())
}
