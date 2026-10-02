//! Write-stall deadline for accepted connections (#21; ADR 0018).
//!
//! The HTTP server asks the response body for the next chunk only when it can write it, so
//! a consumer that stops reading backpressures the provider through TCP and costs the
//! Gateway nothing but the connection. That alone is not a bound: a consumer that never
//! reads would hold the stream permit, the provider connection, and the socket forever.
//! This wrapper bounds it. If a write (or flush) stays pending with no progress for the
//! configured stall deadline, the connection fails with a timeout, the server drops the
//! response, and the response body returns its permits and closes the provider connection.
//!
//! The timer runs only while a write is pending and restarts whenever a write completes, so
//! it measures *no progress*, not total duration; a consumer that keeps making (slow)
//! progress is bounded by the stream lifetime deadline instead. It applies to every
//! response written on an accepted connection (health, errors, buffered JSON, and streams).
//! No task is spawned: the timer is polled inside the write that is pending.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Sleep, sleep};

/// A TCP listener whose accepted connections enforce the write-stall deadline.
#[derive(Debug)]
pub(crate) struct StallListener {
    inner: TcpListener,
    stall: Duration,
}

impl StallListener {
    pub(crate) const fn new(inner: TcpListener, stall: Duration) -> Self {
        Self { inner, stall }
    }
}

impl Listener for StallListener {
    type Io = StallIo<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, addr) = Listener::accept(&mut self.inner).await;
        (StallIo::new(io, self.stall), addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// An IO object that fails a write that makes no progress for `stall`.
#[derive(Debug)]
pub(crate) struct StallIo<T> {
    inner: T,
    stall: Duration,
    /// Running only while a write or flush is pending.
    timer: Option<Pin<Box<Sleep>>>,
}

impl<T> StallIo<T> {
    pub(crate) const fn new(inner: T, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            timer: None,
        }
    }

    /// A write returned `Pending`: run (or continue) the stall timer.
    fn pending<R>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<R>> {
        let stall = self.stall;
        let timer = self.timer.get_or_insert_with(|| Box::pin(sleep(stall)));
        if timer.as_mut().poll(cx).is_ready() {
            self.timer = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "write stalled",
            )));
        }
        Poll::Pending
    }

    /// Progress was made (or the operation failed): the stall clock stops.
    fn settle<R>(&mut self, result: io::Result<R>) -> Poll<io::Result<R>> {
        self.timer = None;
        Poll::Ready(result)
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for StallIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for StallIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(result) => self.settle(result),
            Poll::Pending => self.pending(cx),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(result) => self.settle(result),
            Poll::Pending => self.pending(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(result) => self.settle(result),
            Poll::Pending => self.pending(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_shutdown(cx) {
            Poll::Ready(result) => self.settle(result),
            Poll::Pending => self.pending(cx),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn a_write_with_no_progress_for_the_deadline_fails_with_timed_out() {
        // Nobody reads, so the 8-byte pipe fills and the write stays pending.
        let (near, _far) = duplex(8);
        let mut io = StallIo::new(near, Duration::from_millis(80));
        let started = tokio::time::Instant::now();
        let err = io.write_all(&[0_u8; 4096]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(60), "too early: {took:?}");
        assert!(took < Duration::from_secs(3), "too late: {took:?}");
    }

    #[tokio::test]
    async fn steady_progress_restarts_the_clock_and_completes() {
        let (near, mut far) = duplex(8);
        // The whole transfer takes longer than the deadline, but every pending period is
        // shorter than it because the reader keeps draining.
        let mut io = StallIo::new(near, Duration::from_millis(250));
        let reader = tokio::spawn(async move {
            let mut got = 0_usize;
            let mut buf = [0_u8; 8];
            while got < 64 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                got += far.read(&mut buf).await.unwrap();
            }
            got
        });
        io.write_all(&[7_u8; 64]).await.unwrap();
        assert_eq!(reader.await.unwrap(), 64);
    }

    #[tokio::test]
    async fn an_idle_connection_with_nothing_to_write_is_never_timed_out() {
        let (near, mut far) = duplex(64);
        let mut io = StallIo::new(near, Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(200)).await;
        io.write_all(b"ok").await.unwrap();
        io.flush().await.unwrap();
        let mut buf = [0_u8; 2];
        far.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }
}
