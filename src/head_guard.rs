//! Request-head guard for accepted connections (#25; ADR 0019).
//!
//! The HTTP server resolves `Content-Length` together with `Transfer-Encoding: chunked` by
//! itself (RFC 9112 section 6.3) and a slow or silent peer can hold a connection open
//! before any handler runs. Neither is visible to the request handler, so both are handled
//! here, on the byte stream, before the HTTP parser sees a byte:
//!
//! 1. The first request head on a connection is held back (bounded by [`MAX_HEAD_BYTES`])
//!    until its blank line arrives and is scanned. A head that carries both a
//!    `Content-Length` and a `Transfer-Encoding` field is **ambiguous**: the guard writes a
//!    fixed, local `400 malformed_input` ([`AMBIGUOUS_FRAMING_RESPONSE`], `Connection:
//!    close`), shuts the write side down, and fails the read with an IO error, so the HTTP
//!    parser never sees the head, no handler runs, and nothing is read from the body. A
//!    head that is not finished within the head deadline, or that exceeds the byte bound,
//!    fails the connection silently when late (no response is written to a peer that is
//!    stalling) and gets `431 limit_exceeded` when over the byte bound.
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

/// Largest request head the guard lets through, and the most it ever holds back: the
/// request line, every header line, and the blank line that ends them. A head over this is
/// refused with the fixed `431`, whether it ended inside the bound's last read or never
/// ended, so the outcome does not depend on how the bytes were split across reads. The
/// route's own limits (16 KiB of header names plus values, 8 KiB per value) are far below
/// it and answer first for any head the guard lets through. Measured evidence and the
/// relation between the three numbers: ADR 0023.
pub(crate) const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Most header fields (lines after the request line) the guard lets through. This is the
/// HTTP server's own limit (`hyper` 1.x default of 100, which answers an empty `431`
/// itself); the guard states it so the same fixed `431 limit_exceeded` answers it and a
/// `hyper` upgrade that changed the default cannot silently change the outcome. The
/// boundary test at 100 and 101 fields runs through the real server.
pub(crate) const MAX_HEAD_FIELDS: usize = 100;

const READ_CHUNK: usize = 4096;

/// The complete response written for a head over [`MAX_HEAD_BYTES`] or over
/// [`MAX_HEAD_FIELDS`] fields: `431 limit_exceeded`, as for headers over the route's byte
/// limits.
pub(crate) const HEAD_TOO_LARGE_RESPONSE: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\n\
Content-Type: application/json\r\n\
Cache-Control: no-store\r\n\
Connection: close\r\n\
Content-Length: 35\r\n\
\r\n\
{\"error\":{\"code\":\"limit_exceeded\"}}";

/// The complete response written for an ambiguous head (#43, ADR 0019 follow-up): the
/// fixed `malformed_input` body of the error contract, with no request-derived byte. The
/// pinned HTTP server cannot produce it (it discards the length before any hook runs; see
/// ADR 0021), so the guard that already holds the head writes it.
pub(crate) const AMBIGUOUS_FRAMING_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\
Content-Type: application/json\r\n\
Cache-Control: no-store\r\n\
Connection: close\r\n\
Content-Length: 36\r\n\
\r\n\
{\"error\":{\"code\":\"malformed_input\"}}";

/// What the scan concluded about the bytes held so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadVerdict {
    /// The blank line that ends the head has not arrived.
    Incomplete,
    /// A complete head without a length/transfer-coding conflict.
    Clean,
    /// `Content-Length` and `Transfer-Encoding` are both present.
    Ambiguous,
    /// The head ended, but is longer than [`MAX_HEAD_BYTES`].
    TooLarge,
    /// More than [`MAX_HEAD_FIELDS`] header fields.
    TooManyFields,
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
    let mut fields = 0_usize;
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
                } else if bytes.len().saturating_sub(rest.len()) > MAX_HEAD_BYTES {
                    HeadVerdict::TooLarge
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
        fields = fields.saturating_add(1);
        if fields > MAX_HEAD_FIELDS {
            return HeadVerdict::TooManyFields;
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
    /// The head was ambiguous: write the fixed refusal, close the write side, then fail the
    /// read. The head timer keeps running, so a peer that stops reading cannot hold this.
    Reject {
        response: &'static [u8],
        sent: usize,
        step: RejectStep,
        timer: Pin<Box<Sleep>>,
    },
    /// Everything after the first head passes through untouched.
    Pass,
}

#[derive(Debug, Clone, Copy)]
enum RejectStep {
    Write,
    Flush,
    Shutdown,
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

/// Leave the head phase for the refusal phase, dropping the held bytes and keeping the
/// head timer running.
fn reject(phase: &mut Phase, response: &'static [u8]) {
    if let Phase::Head { timer, .. } = std::mem::replace(phase, Phase::Pass) {
        *phase = Phase::Reject {
            response,
            sent: 0,
            step: RejectStep::Write,
            timer,
        };
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for HeadGuardIo<T> {
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
                Phase::Reject {
                    response,
                    sent,
                    step,
                    timer,
                } => {
                    let polled = match step {
                        RejectStep::Write => {
                            let rest = response.get(*sent..).unwrap_or_default();
                            match Pin::new(&mut this.inner).poll_write(cx, rest) {
                                Poll::Ready(Ok(0)) => {
                                    return refuse(io::ErrorKind::WriteZero, "refusal not written");
                                }
                                Poll::Ready(Ok(n)) => {
                                    *sent = sent.saturating_add(n);
                                    if *sent >= response.len() {
                                        *step = RejectStep::Flush;
                                    }
                                    continue;
                                }
                                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                                Poll::Pending => Poll::Pending,
                            }
                        }
                        RejectStep::Flush => match Pin::new(&mut this.inner).poll_flush(cx) {
                            Poll::Ready(Ok(())) => {
                                *step = RejectStep::Shutdown;
                                continue;
                            }
                            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                            Poll::Pending => Poll::Pending,
                        },
                        RejectStep::Shutdown => match Pin::new(&mut this.inner).poll_shutdown(cx) {
                            // Best effort: the connection is failed either way.
                            Poll::Ready(_) => {
                                return refuse(io::ErrorKind::InvalidData, "request head refused");
                            }
                            Poll::Pending => Poll::Pending,
                        },
                    };
                    if timer.as_mut().poll(cx).is_ready() {
                        return refuse(io::ErrorKind::TimedOut, "request head deadline");
                    }
                    return polled;
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
                            // The held bytes are dropped here; the parser never sees them.
                            reject(&mut this.phase, AMBIGUOUS_FRAMING_RESPONSE);
                            continue;
                        }
                        HeadVerdict::TooLarge | HeadVerdict::TooManyFields => {
                            reject(&mut this.phase, HEAD_TOO_LARGE_RESPONSE);
                            continue;
                        }
                        HeadVerdict::Clean => {}
                        // A truncated head cannot be parsed as a request; hand it over so
                        // the server sees the same end of stream.
                        HeadVerdict::Incomplete if eof => {}
                        HeadVerdict::Incomplete => {
                            if held.len() > MAX_HEAD_BYTES {
                                reject(&mut this.phase, HEAD_TOO_LARGE_RESPONSE);
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

    /// A head of exactly `total` bytes: request line, one padded field, blank line.
    fn head_of(total: usize) -> Vec<u8> {
        let prefix = b"POST / HTTP/1.1\r\nX: ";
        let suffix = b"\r\n\r\n";
        let pad = total.saturating_sub(prefix.len().saturating_add(suffix.len()));
        let mut out = prefix.to_vec();
        out.extend(std::iter::repeat_n(b'a', pad));
        out.extend_from_slice(suffix);
        assert_eq!(out.len(), total);
        out
    }

    /// `n` header fields after the request line.
    fn head_with_fields(n: usize) -> Vec<u8> {
        let mut out = b"POST / HTTP/1.1\r\n".to_vec();
        for _ in 0..n {
            out.extend_from_slice(b"X: 1\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out
    }

    #[test]
    fn scan_bounds_are_exact_on_both_sides() {
        // Head length: the bound itself passes, one byte more does not.
        assert_eq!(inspect_head(&head_of(MAX_HEAD_BYTES)), HeadVerdict::Clean);
        assert_eq!(
            inspect_head(&head_of(MAX_HEAD_BYTES + 1)),
            HeadVerdict::TooLarge
        );
        // Bytes after the blank line (a body) are not part of the head.
        let mut with_body = head_of(MAX_HEAD_BYTES);
        with_body.extend_from_slice(&[b'b'; 4096]);
        assert_eq!(inspect_head(&with_body), HeadVerdict::Clean);
        // Field count: the bound itself passes, one more does not.
        assert_eq!(
            inspect_head(&head_with_fields(MAX_HEAD_FIELDS)),
            HeadVerdict::Clean
        );
        assert_eq!(
            inspect_head(&head_with_fields(MAX_HEAD_FIELDS + 1)),
            HeadVerdict::TooManyFields
        );
        // Detected before the head ends.
        let mut open = head_with_fields(MAX_HEAD_FIELDS + 1);
        open.truncate(open.len() - 2);
        assert_eq!(inspect_head(&open), HeadVerdict::TooManyFields);
        // Ambiguity is judged first when both apply.
        let mut both =
            b"POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n".to_vec();
        both.extend_from_slice(&head_with_fields(MAX_HEAD_FIELDS + 1)[17..]);
        assert_eq!(inspect_head(&both), HeadVerdict::Ambiguous);
    }

    /// The outcome for a head over the bound does not depend on how its bytes were split
    /// across reads: a head that ends within the last read of the bound is refused, one at
    /// the bound is released byte-exact.
    #[tokio::test]
    async fn the_head_bound_does_not_depend_on_read_splitting() {
        for (total, refused) in [(MAX_HEAD_BYTES, false), (MAX_HEAD_BYTES + 1, true)] {
            let head = head_of(total);
            let (near, mut far) = duplex(256 * 1024);
            let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
            far.write_all(&head).await.unwrap();
            if refused {
                let error = io.read(&mut [0_u8; 8]).await.unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                drop(io);
                let mut answer = Vec::new();
                far.read_to_end(&mut answer).await.unwrap();
                assert_eq!(answer, HEAD_TOO_LARGE_RESPONSE);
            } else {
                far.shutdown().await.unwrap();
                let mut got = Vec::new();
                io.read_to_end(&mut got).await.unwrap();
                assert_eq!(got, head);
            }
        }
    }

    #[tokio::test]
    async fn a_head_split_into_single_bytes_is_still_judged_whole() {
        let (near, far) = duplex(64 * 1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        let head = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n";
        let (mut far_read, mut far_write) = tokio::io::split(far);
        tokio::spawn(async move {
            for byte in head {
                far_write.write_all(&[*byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            // Keep the pipe open: the guard must refuse on its own.
            std::future::pending::<()>().await;
        });
        let mut got = [0_u8; 8];
        let error = io.read(&mut got).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The peer got the fixed refusal and then the write side was closed, while its own
        // write side is still open.
        let mut answer = Vec::new();
        far_read.read_to_end(&mut answer).await.unwrap();
        assert_eq!(answer, AMBIGUOUS_FRAMING_RESPONSE);
    }

    /// The refusals are complete, well-formed responses whose declared length is the body.
    #[test]
    fn refusal_responses_are_well_formed_and_fixed() {
        for (response, status, code) in [
            (AMBIGUOUS_FRAMING_RESPONSE, "400", "malformed_input"),
            (HEAD_TOO_LARGE_RESPONSE, "431", "limit_exceeded"),
        ] {
            let text = std::str::from_utf8(response).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            assert!(head.starts_with(&format!("HTTP/1.1 {status} ")));
            assert_eq!(body, format!(r#"{{"error":{{"code":"{code}"}}}}"#));
            let declared = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap();
            assert_eq!(declared.parse::<usize>().unwrap(), body.len());
            for field in [
                "Connection: close",
                "Content-Type: application/json",
                "Cache-Control: no-store",
            ] {
                assert!(head.lines().any(|l| l == field), "{field}");
            }
        }
    }

    /// An ambiguous head is refused even when pipelined bytes follow it: nothing after the
    /// head is released to the reader, and the refusal is repeated on every later read.
    #[tokio::test]
    async fn an_ambiguous_head_releases_nothing_to_the_reader() {
        let (near, mut far) = duplex(64 * 1024);
        let mut io = HeadGuardIo::new(near, Duration::from_secs(30));
        far.write_all(
            b"POST / HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n\
              GET /healthz HTTP/1.1\r\n\r\n",
        )
        .await
        .unwrap();
        let mut got = [0_u8; 256];
        for _ in 0..3 {
            let error = io.read(&mut got).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        assert!(got.iter().all(|b| *b == 0), "no byte was handed over");
        drop(io);
        let mut answer = Vec::new();
        far.read_to_end(&mut answer).await.unwrap();
        assert_eq!(answer, AMBIGUOUS_FRAMING_RESPONSE, "exactly one refusal");
    }

    /// A peer that does not read its refusal cannot hold the connection past the head
    /// deadline: the write is pending (tiny pipe) and the absolute timer ends it.
    #[tokio::test]
    async fn a_peer_that_never_reads_the_refusal_is_cut_at_the_head_deadline() {
        let (near, mut far) = duplex(8);
        let mut io = HeadGuardIo::new(near, Duration::from_millis(200));
        let head = b"POST / HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n";
        // Only 8 bytes fit in the pipe at a time, so feed the head from a task.
        let feeder = tokio::spawn(async move {
            let _ = far.write_all(head).await;
            std::future::pending::<()>().await;
        });
        let error = io.read(&mut [0_u8; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        feeder.abort();
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
        drop(io);
        let mut answer = Vec::new();
        far.read_to_end(&mut answer).await.unwrap();
        assert_eq!(answer, HEAD_TOO_LARGE_RESPONSE);
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
