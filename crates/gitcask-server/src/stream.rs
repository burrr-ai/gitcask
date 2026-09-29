//! Streaming bridges between axum/hyper bodies and tokio `AsyncRead`/`AsyncWrite`.
//!
//! * Incoming request body -> `AsyncRead` (with optional gzip inflate).
//! * Outgoing response: the write half of a tokio duplex pipe rendered as a
//!   hyper `Body` via `Body::from_stream`, so git pkt-line / pack output streams
//!   straight to the client with no full buffering.

use std::io;

use axum::body::Body;
use futures::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::{ReaderStream, StreamReader};

/// Convert an axum request body into an `AsyncRead`. Map errors to io::Error.
pub fn body_to_async_read(body: Body) -> impl AsyncRead + Unpin + Send {
    let stream = body
        .into_data_stream()
        .map(|res| res.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string())));
    StreamReader::new(stream)
}

/// Wrap an `AsyncRead` in gzip decompression when `content_encoding` is `gzip`.
/// Returns the original reader otherwise. The gzip decoder requires `AsyncBufRead`,
/// so the reader is wrapped in a `BufReader`. The decoder accepts concatenated
/// members, so it reads the body to its end: its EOF is the request body's EOF
/// (over HTTP/1.x receive-pack starts its response only after that).
pub fn maybe_gunzip<R: AsyncRead + Unpin + Send + 'static>(
    content_encoding: Option<&str>,
    reader: R,
) -> Box<dyn AsyncRead + Unpin + Send> {
    match content_encoding
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("gzip") => {
            let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(
                tokio::io::BufReader::new(reader),
            );
            decoder.multiple_members(true);
            Box::new(decoder)
        }
        _ => Box::new(reader),
    }
}

/// Convert an `AsyncRead` into an axum `Body` (streamed, chunked).
pub fn body_from_async_read<R: AsyncRead + Unpin + Send + 'static>(reader: R) -> Body {
    Body::from_stream(ReaderStream::new(reader))
}

/// A duplex pipe: write on the returned `DuplexStream` (impl `AsyncWrite`),
/// read the resulting `Body` on the other side. Use when an API hands us an
/// `AsyncWrite` to fill (e.g. `LocalRepo::upload_pack`). Drop the writer to
/// signal EOF to the reader.
pub fn write_body_pipe(buf: usize) -> (tokio::io::DuplexStream, Body) {
    let (a, b) = tokio::io::duplex(buf);
    (a, Body::from_stream(ReaderStream::new(b)))
}

/// An `AsyncWrite` that never blocks: each write becomes one message on an
/// unbounded channel, read back in order from the returned receiver and copied
/// to the response by [`forward`]. A side-band receive-pack writes its whole
/// response through it (banner, sync narration, report), so when the response
/// may start is decided only by when [`forward`] starts: at once, or after the
/// request body has ended, replaying what was said meanwhile. Until then the
/// channel holds narration only, which is rate-limited at the source (progress
/// at most once a second, a heartbeat every 5 s), so it stays small for any
/// upload inside `server.request_timeout`.
pub fn narration_channel() -> (ChannelWriter, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (ChannelWriter(tx), rx)
}

/// Copy [`narration_channel`] messages to `out` until every writer is dropped,
/// then flush and shut `out` down (the end of the streamed response). A failed
/// write (client gone) stops the copy; the writers then see `BrokenPipe`.
pub async fn forward<W: AsyncWrite + Unpin>(
    mut lines: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    mut out: W,
) {
    use tokio::io::AsyncWriteExt;
    while let Some(line) = lines.recv().await {
        if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
            return;
        }
    }
    let _ = out.shutdown().await;
}

/// The write half of [`narration_channel`]; clones feed the same channel.
#[derive(Clone)]
pub struct ChannelWriter(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

impl AsyncWrite for ChannelWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::task::Poll::Ready(match self.0.send(buf.to_vec()) {
            Ok(()) => Ok(buf.len()),
            Err(_) => Err(io::ErrorKind::BrokenPipe.into()),
        })
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A simple `AsyncWrite` that collects bytes into a `Vec<u8>`. Used to render
/// small pkt-line responses (report-status, ls-refs) into a buffer.
pub struct VecWriter(pub Vec<u8>);

impl VecWriter {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl AsyncWrite for VecWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.get_mut().0.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Writes made before the reader looks come back whole and in order, and
    /// the channel ends when the writer is dropped.
    #[tokio::test]
    async fn narration_channel_replays_in_order() {
        let (mut w, mut rx) = narration_channel();
        w.write_all(b"one\n").await.unwrap();
        w.write_all(b"two\n").await.unwrap();
        drop(w);
        let mut out = Vec::new();
        while let Some(chunk) = rx.recv().await {
            out.extend(chunk);
        }
        assert_eq!(out, b"one\ntwo\n");
    }

    /// `forward` copies everything written before it started, then follows
    /// live writes, and ends the output when the last writer is dropped.
    #[tokio::test]
    async fn forward_replays_then_follows_live_writes() {
        use tokio::io::AsyncReadExt;
        let (mut w, rx) = narration_channel();
        let mut w2 = w.clone();
        w.write_all(b"banner\n").await.unwrap();
        w2.write_all(b"buffered\n").await.unwrap();
        let (pipe, mut client) = tokio::io::duplex(1024);
        let copy = tokio::spawn(forward(rx, pipe));
        w.write_all(b"live\n").await.unwrap();
        drop(w);
        drop(w2);
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        copy.await.unwrap();
        assert_eq!(out, "banner\nbuffered\nlive\n");
    }

    #[tokio::test]
    async fn narration_channel_write_fails_once_the_reader_is_gone() {
        let (mut w, rx) = narration_channel();
        drop(rx);
        assert!(w.write_all(b"late\n").await.is_err());
    }
}
