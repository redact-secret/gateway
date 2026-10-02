//! Request-head guard for accepted connections (#25; ADR 0019).
//!
//! The HTTP server resolves `Content-Length` together with `Transfer-Encoding: chunked` by
//! itself (RFC 9112 section 6.3) and a slow or silent peer can hold a connection open
//! before any handler runs. Neither is visible to the request handler, so both are handled
//! here, on the byte stream, before the HTTP parser sees a byte:
//!
//! 1. The first request head on a connection is held back (bounded by [`MAX_HEAD_BYTES`])
//!    until its blank line arrives and is scanned. A head that carries both a
//!    `Content-Length` and a `Transfer-Encoding` field is **ambiguous**: the connection is
//!    failed with an IO error and the HTTP parser never sees it, so no handler runs and
//!    nothing is read from the body. A head that is not finished within the head deadline,
//!    or that exceeds the byte bound, fails the connection the same way.
//! 2. Only the first head is inspected. That is sufficient because the server marks every
//!    response `Connection: close` ([`close_after_response`]), so a connection carries one
//!    request: bytes after that request (a pipelined or smuggled second message) are never
//!    parsed as a request.
//!
//! No task is spawned: the deadline is a timer polled inside the read that is pending.
//! The scan looks only at field names, never at values, and nothing it sees is stored
//! beyond the held head or logged. This is a structural check, not a general HTTP parser;
//! everything else about the head is still judged by the HTTP server and the route.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Sleep, sleep};

/// Largest request head held back for inspection. The route's own header limit is 16 KiB
/// (`431`); this bound only keeps the held bytes finite for heads that never end.
pub(crate) const MAX_HEAD_BYTES: usize = 64 * 1024;

const READ_CHUNK: usize = 4096;

/// What the scan concluded about the bytes held so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadVerdict {
    /// The blank line that ends the head has not arrived.
    Incomplete,
    /// A complete head without a length/transfer-coding conflict.
    Clean,
    /// `Content-Length` and `Transfer-Encoding` are both present.
    Ambiguous,
}

/// Scan the start of a connection for the head and its framing fields. Line endings are
/// read as `\n` with an optional preceding `\r`, which is the more permissive of what the
/// HTTP parser accepts, so a head the parser would end is ended here too. Leading blank
/// lines before the request line are skipped, as the parser does.
pub(crate) fn inspect_head(bytes: &[u8]) -> HeadVerdict {
    let mut rest = bytes;
    let mut in_head = false;
    let mut length = false;
    let mut coding = false;
    loop {
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            return HeadVerdict::Incomplete;
        };
        let (line, tail) = (
            rest.get(..end).unwrap_or_default(),
            rest.get(end.saturating_add(1)..).unwrap_or_default(),
        );
        rest = tail;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if in_head {
                return if length && coding {
                    HeadVerdict::Ambiguous
                } else {
                    HeadVerdict::Clean
                };
            }
            continue;
        }
        if !in_head {
            // The request line.
            in_head = true;
            continue;
        }
        let name = line
            .iter()
            .position(|b| *b == b':')
            .and_then(|colon| line.get(..colon))
            .unwrap_or_default()
            .trim_ascii();
        length |= name.eq_ignore_ascii_case(b"content-length");
        coding |= name.eq_ignore_ascii_case(b"transfer-encoding");
        if length && coding {
            return HeadVerdict::Ambiguous;
        }
    }
}

/// Mark a response so the server closes the connection after it. Every response carries it,
/// which is what limits a connection to one request (see the module documentation).
pub(crate) async fn close_after_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// A listener whose accepted connections run the head guard.
#[derive(Debug)]
pub(crate) struct HeadGuardListener<L> {
    inner: L,
    deadline: Duration,
}

impl<L> HeadGuardListener<L> {
    pub(crate) const fn new(inner: L, deadline: Duration) -> Self {
        Self { inner, deadline }
    }
}

impl<L> Listener for HeadGuardListener<L>
where
    L: Listener<Addr = SocketAddr>,
    L::Io: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Io = HeadGuardIo<L::Io>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, addr) = self.inner.accept().await;
        (HeadGuardIo::new(io, self.deadline), addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[derive(Debug)]
enum Phase {
    /// Collecting the first head. The timer started when the connection was accepted.
    Head {
        held: Vec<u8>,
        timer: Pin<Box<Sleep>>,
    },
    /// The head was accepted; hand the held bytes to the reader first.
    Release { held: Vec<u8>, from: usize },
    /// Everything after the first head passes through untouched.
    Pass,
}

/// An IO object that vets the first request head before the HTTP server reads it.
#[derive(Debug)]
pub(crate) struct HeadGuardIo<T> {
    inner: T,
    phase: Phase,
}

impl<T> HeadGuardIo<T> {
    pub(crate) fn new(inner: T, deadline: Duration) -> Self {
        Self {
            inner,
            phase: Phase::Head {
                held: Vec::new(),
                timer: Box::pin(sleep(deadline)),
            },
        }
    }
}

fn refuse(kind: io::ErrorKind, why: &'static str) -> Poll<io::Result<()>> {
    Poll::Ready(Err(io::Error::new(kind, why)))
}

impl<T: AsyncRead + Unpin> AsyncRead for HeadGuardIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            match &mut this.phase {
                Phase::Pass => return Pin::new(&mut this.inner).poll_read(cx, buf),
                Phase::Release { held, from } => {
                    let rest = held.get(*from..).unwrap_or_default();
                    let n = rest.len().min(buf.remaining());
                    buf.put_slice(rest.get(..n).unwrap_or_default());
                    *from = from.saturating_add(n);
                    if *from >= held.len() {
                        this.phase = Phase::Pass;
                    }
                    return Poll::Ready(Ok(()));
                }
                Phase::Head { held, timer } => {
                    let mut chunk = [0_u8; READ_CHUNK];
                    let mut read = ReadBuf::new(&mut chunk);
                    match Pin::new(&mut this.inner).poll_read(cx, &mut read) {
                        Poll::Pending => {
                            if timer.as_mut().poll(cx).is_ready() {
                                return refuse(io::ErrorKind::TimedOut, "request head deadline");
                            }
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Ready(Ok(())) => {}
                    }
                    let filled = read.filled();
                    let eof = filled.is_empty();
                    held.extend_from_slice(filled);
                    match inspect_head(held) {
                        HeadVerdict::Ambiguous => {
                            return refuse(io::ErrorKind::InvalidData, "ambiguous request framing");
                        }
                        HeadVerdict::Clean => {}
                        // A truncated head cannot be parsed as a request; hand it over so
                        // the server sees the same end of stream.
                        HeadVerdict::Incomplete if eof => {}
                        HeadVerdict::Incomplete => {
                            if held.len() > MAX_HEAD_BYTES {
                                return refuse(
                                    io::ErrorKind::InvalidData,
                                    "request head too large",
                                );
                            }
                            continue;
                        }
                    }
                    let held = std::mem::take(held);
                    this.phase = Phase::Release { held, from: 0 };
                }
            }
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for HeadGuardIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[test]
    fn scan_classifies_heads() {
        let clean = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\nab";
        assert_eq!(inspect_head(clean), HeadVerdict::Clean);
        assert_eq!(
            inspect_head(b"POST / HTTP/1.1\r\nHost: x\r\n"),
            HeadVerdict::Incomplete
        );
        assert_eq!(inspect_head(b""), HeadVerdict::Incomplete);
        for ambiguous in [
            &b"POST / HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\nCONTENT-LENGTH: 2\r\n\r\n",
            // Bare LF line endings end lines and the head as the parser would.
            b"POST / HTTP/1.1\nContent-Length: 2\nTransfer-Encoding: chunked\n\n",
            // Whitespace around the name does not hide it.
            b"POST / HTTP/1.1\r\nContent-Length : 2\r\nTransfer-Encoding\t: chunked\r\n\r\n",
            // Detected before the head ends.
            b"POST / HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: x\r\n",
            // A leading blank line before the request line is skipped.
            b"\r\nPOST / HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert_eq!(inspect_head(ambiguous), HeadVerdict::Ambiguous);
        }
        // The names appearing in values, or only the body, are not a conflict.
        let in_value = b"POST / HTTP/1.1\r\nX: Content-Length Transfer-Encoding\r\n\r\n";
        assert_eq!(inspect_head(in_value), HeadVerdict::Clean);
        let in_body = b"POST / HTTP/1.1\r\nContent-Length: 40\r\n\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(inspect_head(in_body), HeadVerdict::Clean);
    }

    #[tokio::test]
    async fn a_clean_head_and_everything_after_it_pass_through_byte_exact() {
        let (near, mut far) = duplex(64 * 1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        let head = b"POST / HTTP/1.1\r\nContent-Length: 4\r\n\r\n";
        far.write_all(head).await.unwrap();
        far.write_all(b"bodyPIPELINED").await.unwrap();
        far.shutdown().await.unwrap();
        let mut got = Vec::new();
        io.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, [&head[..], b"bodyPIPELINED"].concat());
    }

    #[tokio::test]
    async fn a_head_split_into_single_bytes_is_still_judged_whole() {
        let (near, mut far) = duplex(64 * 1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n";
        tokio::spawn(async move {
            for byte in head {
                far.write_all(&[*byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            // Keep the pipe open: the guard must refuse on its own.
            std::future::pending::<()>().await;
        });
        let mut got = [0_u8; 8];
        let error = io.read(&mut got).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_silent_or_trickling_peer_is_cut_at_the_head_deadline() {
        // Nothing is ever sent.
        let (near, _far) = duplex(64);
        let mut io = HeadGuardIo::new(near, Duration::from_millis(100));
        let error = io.read(&mut [0_u8; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // One byte now and then never finishes the head, and the deadline is absolute.
        let (near, mut far) = duplex(64);
        let mut io = HeadGuardIo::new(near, Duration::from_millis(300));
        let feeder = tokio::spawn(async move {
            loop {
                if far.write_all(b"A").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let error = io.read(&mut [0_u8; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        feeder.abort();
    }

    #[tokio::test]
    async fn a_head_that_never_ends_is_bounded() {
        let (near, mut far) = duplex(256 * 1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        let line = format!("X: {}\r\n", "a".repeat(1000));
        let mut sent = b"POST / HTTP/1.1\r\n".to_vec();
        while sent.len() <= MAX_HEAD_BYTES + READ_CHUNK {
            sent.extend_from_slice(line.as_bytes());
        }
        far.write_all(&sent).await.unwrap();
        let mut got = [0_u8; 8];
        let error = io.read(&mut got).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_truncated_head_ends_like_any_truncated_request() {
        let (near, mut far) = duplex(1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        far.write_all(b"POST / HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        far.shutdown().await.unwrap();
        let mut got = Vec::new();
        io.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"POST / HTTP/1.1\r\nHost: x\r\n");
    }

    #[tokio::test]
    async fn writes_are_not_held_back() {
        let (near, mut far) = duplex(1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        io.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        io.flush().await.unwrap();
        let mut got = [0_u8; 19];
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"HTTP/1.1 200 OK\r\n\r\n");
    }
}
