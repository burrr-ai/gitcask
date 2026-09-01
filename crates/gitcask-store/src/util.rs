use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use crate::{ByteStream, Result, StoreError};

/// Collect a byte stream. `size_hint` pre-allocates.
pub async fn collect(mut body: ByteStream, size_hint: usize) -> Result<Bytes> {
    let mut first: Option<Bytes> = None;
    let mut buf: Option<BytesMut> = None;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        match (&mut first, &mut buf) {
            (None, None) => first = Some(chunk),
            (Some(_), None) => {
                let f = first.take().unwrap();
                let mut b = BytesMut::with_capacity(size_hint.max(f.len() + chunk.len()));
                b.extend_from_slice(&f);
                b.extend_from_slice(&chunk);
                buf = Some(b);
            }
            (_, Some(b)) => b.extend_from_slice(&chunk),
        }
    }
    Ok(match (first, buf) {
        (Some(f), None) => f,
        (_, Some(b)) => b.freeze(),
        (None, None) => Bytes::new(),
    })
}

/// Wrap a single `Bytes` as a stream.
pub fn once(b: Bytes) -> ByteStream {
    Box::pin(futures::stream::once(async move { Ok(b) }))
}

/// Stream a local file in `chunk` sized pieces, optionally a byte range.
pub fn file_stream(
    path: std::path::PathBuf,
    range: Option<std::ops::Range<u64>>,
    chunk: usize,
) -> ByteStream {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    return async_stream_file(path, range, chunk)
        .map(|r| r.map_err(StoreError::other))
        .boxed();

    fn async_stream_file(
        path: std::path::PathBuf,
        range: Option<std::ops::Range<u64>>,
        chunk: usize,
    ) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
        futures::stream::unfold(State::Init { path, range, chunk }, |st| async move {
            match st {
                State::Init { path, range, chunk } => {
                    let mut f = match tokio::fs::File::open(&path).await {
                        Ok(f) => f,
                        Err(e) => return Some((Err(e), State::Done)),
                    };
                    let (start, remaining) = match range {
                        Some(r) => (r.start, r.end.saturating_sub(r.start)),
                        None => match f.metadata().await {
                            Ok(m) => (0, m.len()),
                            Err(e) => return Some((Err(e), State::Done)),
                        },
                    };
                    if start > 0 {
                        if let Err(e) = f.seek(std::io::SeekFrom::Start(start)).await {
                            return Some((Err(e), State::Done));
                        }
                    }
                    read_next(f, remaining, chunk).await
                }
                State::Reading {
                    f,
                    remaining,
                    chunk,
                } => read_next(f, remaining, chunk).await,
                State::Done => None,
            }
        })
    }
    async fn read_next(
        mut f: tokio::fs::File,
        remaining: u64,
        chunk: usize,
    ) -> Option<(std::io::Result<Bytes>, State)> {
        if remaining == 0 {
            return None;
        }
        let want = (chunk as u64).min(remaining) as usize;
        let mut buf = BytesMut::with_capacity(want);
        // read_buf reads at most capacity; loop until we get `want` or EOF.
        while buf.len() < want {
            match f.read_buf(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Some((Err(e), State::Done)),
            }
        }
        if buf.is_empty() {
            return None;
        }
        let n = buf.len() as u64;
        Some((
            Ok(buf.freeze()),
            State::Reading {
                f,
                remaining: remaining - n,
                chunk,
            },
        ))
    }
    enum State {
        Init {
            path: std::path::PathBuf,
            range: Option<std::ops::Range<u64>>,
            chunk: usize,
        },
        Reading {
            f: tokio::fs::File,
            remaining: u64,
            chunk: usize,
        },
        Done,
    }
}

/// Exponential backoff with full jitter. `attempt` starts at 0.
pub fn backoff(
    attempt: u32,
    base: std::time::Duration,
    max: std::time::Duration,
) -> std::time::Duration {
    use rand::Rng;
    let exp = base.saturating_mul(1u32 << attempt.min(16));
    let cap = exp.min(max);
    let jitter = rand::rng().random_range(0..=cap.as_millis() as u64);
    std::time::Duration::from_millis(jitter)
}

/// Retry a store operation on [`StoreError::Retryable`] with exponential full jitter.
/// `max_retries` counts retries after the first attempt; zero performs one attempt.
pub async fn with_retry<T, F, Fut>(
    op: &'static str,
    key: &str,
    max_retries: u32,
    mut make: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut retries = 0;
    loop {
        // Boxing keeps large SDK request futures out of every caller's state
        // machine while they wait through the retry loop.
        match Box::pin(make()).await {
            Err(e) if e.is_retryable() && retries < max_retries => {
                retries += 1;
                let d = backoff(
                    retries - 1,
                    std::time::Duration::from_millis(100),
                    std::time::Duration::from_secs(2),
                );
                tracing::warn!(op, key, attempt = retries, error = %e, "retrying store operation");
                metrics::counter!("gitcask_store_retries_total", "op" => op).increment(1);
                tokio::time::sleep(d).await;
            }
            other => return other,
        }
    }
}

/// Percent-encode an object key for use in a URL path: slashes stay slashes (they are
/// the key's own separators), every other byte outside the unreserved set is encoded.
pub fn encode_path(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
