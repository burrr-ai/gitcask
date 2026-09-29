//! receive-pack reads the whole request body before its response starts.
//!
//! A proxy that forwards the request body over HTTP/1 may stop forwarding it
//! once the upstream answers: in production (AWS ALB → HTTP/1 target) a
//! side-band-64k push whose response headers and band-2 banner went out right
//! after the ref commands stalled forever in the pack body read. These tests
//! drive a raw HTTP/1.1 connection so the body can be paced and the first
//! response byte timed against the last request byte.
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
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "-q"])
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
            .write_all(b"refs/heads/main\n")?;
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
    let cmd = format!("{} {head} refs/heads/main\0{caps}\n", "0".repeat(40));
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

/// Walk `dir` and return every file whose name marks an in-flight or leaked
/// pack reception (anything but a published `pack-<checksum>.*`).
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
            if name.starts_with("gitcask-ingest-")
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
