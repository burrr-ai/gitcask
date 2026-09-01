//! HTTP contract of immutable store objects (LFS here) using the shared
//! `static_object` path and of the embedded UI assets: strong ETags, 304,
//! Range/If-Range, HEAD, Content-Length, precompressed encodings.

mod harness;

use anyhow::Result;
use harness::Server;
use reqwest::StatusCode;
use sha2::{Digest, Sha256};

fn hdr(r: &reqwest::Response, name: &str) -> String {
    r.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lfs_object_full_http_contract() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?;
    let client = reqwest::Client::new();
    let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let oid = hex::encode(Sha256::digest(&body));
    let url = format!("{}/o/r.git/info/lfs/objects/{oid}", server.base_url);
    let put = client.put(&url).body(body.clone()).send().await?;
    assert!(put.status().is_success(), "put: {}", put.status());

    // Plain GET: 200, strong quoted ETag, Content-Length, immutable, ranges.
    let r = client.get(&url).send().await?;
    assert_eq!(r.status(), StatusCode::OK);
    let etag = hdr(&r, "etag");
    assert!(
        etag.starts_with('"') && etag.ends_with('"') && etag.len() > 2,
        "etag {etag:?}"
    );
    assert_eq!(hdr(&r, "content-length"), body.len().to_string());
    assert_eq!(hdr(&r, "accept-ranges"), "bytes");
    assert!(hdr(&r, "cache-control").contains("immutable"));
    assert_eq!(r.bytes().await?.as_ref(), &body[..]);

    // If-None-Match → 304 (also with a list / weak prefix).
    for inm in [
        etag.clone(),
        format!("\"x\", {etag}"),
        format!("W/{etag}"),
        "*".into(),
    ] {
        let r = client
            .get(&url)
            .header("If-None-Match", inm.clone())
            .send()
            .await?;
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED, "If-None-Match: {inm}");
        assert_eq!(hdr(&r, "etag"), etag);
    }
    let r = client
        .get(&url)
        .header("If-None-Match", "\"nope\"")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);

    // HEAD: metadata only.
    let r = client.head(&url).send().await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "etag"), etag);
    assert_eq!(hdr(&r, "content-length"), body.len().to_string());
    assert!(r.bytes().await?.is_empty());

    // Range: closed, open-ended, suffix.
    let r = client
        .get(&url)
        .header("Range", "bytes=100-199")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        hdr(&r, "content-range"),
        format!("bytes 100-199/{}", body.len())
    );
    assert_eq!(hdr(&r, "content-length"), "100");
    assert_eq!(r.bytes().await?.as_ref(), &body[100..200]);
    let r = client
        .get(&url)
        .header("Range", "bytes=9900-")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        hdr(&r, "content-range"),
        format!("bytes 9900-9999/{}", body.len())
    );
    assert_eq!(r.bytes().await?.as_ref(), &body[9900..]);
    let r = client.get(&url).header("Range", "bytes=-50").send().await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        hdr(&r, "content-range"),
        format!("bytes 9950-9999/{}", body.len())
    );
    assert_eq!(r.bytes().await?.as_ref(), &body[9950..]);
    // Past the end: clamp (RFC 9110) / unsatisfiable.
    let r = client
        .get(&url)
        .header("Range", "bytes=9990-20000")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(hdr(&r, "content-length"), "10");
    let r = client
        .get(&url)
        .header("Range", "bytes=20000-")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(hdr(&r, "content-range"), format!("bytes */{}", body.len()));
    // Multi-range is answered as a full 200.
    let r = client
        .get(&url)
        .header("Range", "bytes=0-1,5-6")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);

    // If-Range: matching ETag → 206; stale ETag → full 200.
    let r = client
        .get(&url)
        .header("Range", "bytes=0-9")
        .header("If-Range", etag.clone())
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    let r = client
        .get(&url)
        .header("Range", "bytes=0-9")
        .header("If-Range", "\"stale\"")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "content-length"), body.len().to_string());

    // Range + If-None-Match hit → 304 wins.
    let r = client
        .get(&url)
        .header("Range", "bytes=0-9")
        .header("If-None-Match", etag.clone())
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);

    // Unknown object → 404, HEAD too.
    let missing = format!(
        "{}/o/r.git/info/lfs/objects/{}",
        server.base_url,
        "0".repeat(64)
    );
    assert_eq!(
        client.get(&missing).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client.head(&missing).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_list_sse_streams_events() -> Result<()> {
    let server = Server::start().await?;
    server.put_repo("o", "r").await?;
    let src = harness::TestRepo::synthetic(1, 1)?;
    harness::git_in(&src, &["commit", "--allow-empty", "-m", "one"])?;
    harness::git_in(&src, &["branch", "-M", "main"])?;
    for b in ["feature/a", "feature/b", "hotfix/x"] {
        harness::git_in(&src, &["branch", b])?;
    }
    harness::git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("o", "r")],
    )?;
    harness::git_in(&src, &["push", "origin", "--all"])?;
    let client = reqwest::Client::new();
    let r = client
        .get(format!(
            "{}/o/r/api/refs/branches?q=feature&n=10",
            server.base_url
        ))
        .header("Accept", "text/event-stream")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(hdr(&r, "content-type").starts_with("text/event-stream"));
    assert_eq!(
        hdr(&r, "content-encoding"),
        "",
        "SSE must not be compressed/buffered"
    );
    let text = r.text().await?;
    let refs: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("data: {\"name\""))
        .collect();
    assert_eq!(refs.len(), 2, "{text}");
    assert!(text.contains("event: ref\ndata: {\"name\":\"feature/a\""));
    assert!(
        text.trim_end()
            .ends_with("event: done\ndata: {\"more\":false}")
    );
    Ok(())
}

/// Edge offload (deploy/nginx.conf.example): when the nginx in front announces
/// `X-Gitcask-Capabilities: accel-redirect` and `server.accel_redirect` is on, a
/// static object is answered with `X-Accel-Redirect: /_store/` (no body; the edge
/// fetches `X-Gitcask-Store-Url` with `X-Gitcask-Store-Authorization` when given, and
/// caches under `X-Gitcask-Store-Key`); validators/304 and HEAD
/// stay ours; without the capability header (a direct client) or without the
/// config the bytes still stream from here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accel_redirect_offloads_static_objects_to_the_edge() -> Result<()> {
    let server = Server::start_with_tweak(|c| c.server.accel_redirect = true).await?;
    server
        .store
        .fake_object_urls
        .store(true, std::sync::atomic::Ordering::Relaxed);
    server.put_repo("o", "r").await?;
    let client = reqwest::Client::new();
    let body: Vec<u8> = (0..5_000u32).map(|i| (i % 13) as u8).collect();
    let oid = hex::encode(Sha256::digest(&body));
    let url = format!("{}/o/r.git/info/lfs/objects/{oid}", server.base_url);
    assert!(
        client
            .put(&url)
            .body(body.clone())
            .send()
            .await?
            .status()
            .is_success()
    );

    // Direct client: no capability header → bytes from us.
    let r = client.get(&url).send().await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-accel-redirect"), "");
    assert_eq!(r.bytes().await?.as_ref(), &body[..]);

    // Through the edge: redirect, no body, our validators.
    let r = client
        .get(&url)
        .header("X-Gitcask-Capabilities", "accel-redirect, something-else")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-accel-redirect"), "/_store/");
    let store_url = hdr(&r, "x-gitcask-store-url");
    assert!(
        store_url.starts_with("https://storage.example.test/test-bucket/")
            && store_url.ends_with(&oid),
        "url {store_url:?}"
    );
    assert!(hdr(&r, "x-gitcask-store-key").ends_with(&oid));
    let etag = hdr(&r, "etag");
    assert!(etag.starts_with('"'));
    // The edge re-emits this as the client-visible ETag (nginx drops ETag across X-Accel-Redirect
    // and would pass the bucket's md5/crc form instead, breaking If-Range against our HEAD's value).
    assert_eq!(hdr(&r, "x-gitcask-etag"), etag);
    // Exactly one Cache-Control, the app's: nginx carries it over the internal redirect and
    // must add none of its own (the edge hides the bucket's and has no add_header for it).
    let cc: Vec<_> = r.headers().get_all("cache-control").iter().collect();
    assert_eq!(cc.len(), 1, "one Cache-Control header, got {cc:?}");
    assert_eq!(
        hdr(&r, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    // The edge fetches with OUR credentials: the accel answer carries them (nginx
    // keeps this upstream header across the internal redirect and never forwards
    // it to the client) — no edge token, no refresher, no periodic reload.
    assert_eq!(
        hdr(&r, "x-gitcask-store-authorization"),
        "Bearer test-store-access-token"
    );
    assert!(r.bytes().await?.is_empty());

    // 304 is still decided here (no redirect), HEAD too.
    let r = client
        .get(&url)
        .header("X-Gitcask-Capabilities", "accel-redirect")
        .header("If-None-Match", etag.clone())
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(hdr(&r, "x-accel-redirect"), "");
    let r = client
        .head(&url)
        .header("X-Gitcask-Capabilities", "accel-redirect")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-accel-redirect"), "");
    assert_eq!(hdr(&r, "content-length"), body.len().to_string());

    // Unknown object: 404, never a redirect into the bucket.
    let r = client
        .get(format!(
            "{}/o/r.git/info/lfs/objects/{}",
            server.base_url,
            "0".repeat(64)
        ))
        .header("X-Gitcask-Capabilities", "accel-redirect")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Config off: the header is ignored.
    let plain = server
        .start_sibling_with(|c| c.server.accel_redirect = false)
        .await?;
    let r = client
        .get(format!("{}/o/r.git/info/lfs/objects/{oid}", plain.base_url))
        .header("X-Gitcask-Capabilities", "accel-redirect")
        .send()
        .await?;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-accel-redirect"), "");
    assert_eq!(r.bytes().await?.len(), body.len());
    Ok(())
}

/// `/healthz` carries the build sha the ssd-host version follower compares against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_carries_the_build_version() -> Result<()> {
    let server = Server::start().await?; // auth mode none → everyone is a principal
    let client = reqwest::Client::new();
    let r = client
        .get(format!("{}/healthz", server.base_url))
        .send()
        .await?;
    let v: serde_json::Value = r.json().await?;
    assert_eq!(v["status"], "ok");
    assert!(v["version"].as_str().is_some_and(|s| !s.is_empty()));

    Ok(())
}
