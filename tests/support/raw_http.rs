//! Tiny HTTP/1.1 client over a raw socket, used by harness tests so they depend on
//! nothing but tokio. It speaks `Connection: close` only. Not a general client.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Synthetic provider credential added by [`post`] and [`post_chunked`] when the caller
/// supplies no `Authorization` header (#24 requires one on the Chat Completions route).
/// Obviously fake and revoked-looking; never a real key.
pub const DEFAULT_SYNTHETIC_AUTH: &str = "Bearer sk-SYNTHETIC-REVOKED-DEFAULT-NOT-A-KEY";

/// Build a `POST` request with a `Content-Length` body. An `Authorization` header is added
/// unless `headers` already names one; use [`post_exact`] to send exactly `headers`.
#[must_use]
pub fn post(path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    if headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("authorization"))
    {
        return post_exact(path, headers, body);
    }
    let mut with_auth = headers.to_vec();
    with_auth.push(("Authorization", DEFAULT_SYNTHETIC_AUTH));
    post_exact(path, &with_auth, body)
}

/// Build a `POST` request with exactly the given headers (plus `Host`, `Connection:
/// close`, and a `Content-Length` for `body`).
#[must_use]
pub fn post_exact(path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Build a chunked `POST` whose body is split at the given chunk boundaries.
#[must_use]
pub fn post_chunked(path: &str, chunks: &[&[u8]]) -> Vec<u8> {
    let mut bytes = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nTransfer-Encoding: chunked\r\nAuthorization: {DEFAULT_SYNTHETIC_AUTH}\r\n\r\n"
    )
    .into_bytes();
    for chunk in chunks {
        bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"0\r\n\r\n");
    bytes
}

/// Send `request` and read until the server closes or `timeout` passes. Returns whatever
/// bytes arrived (possibly none) and whether the read ended by timeout.
pub async fn exchange(
    addr: SocketAddr,
    request: &[u8],
    timeout: Duration,
) -> io::Result<(Vec<u8>, bool)> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(request).await?;
    let mut out = Vec::new();
    let mut tmp = [0_u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut tmp)).await {
            Err(_) => return Ok((out, true)),
            Ok(Ok(0)) => return Ok((out, false)),
            Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
            // A reset after partial data is a normal outcome for disconnect tests.
            Ok(Err(_)) => return Ok((out, false)),
        }
    }
}

/// A parsed response (status, headers, de-chunked body).
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// True when a declared `Content-Length` was not satisfied or chunked framing did not
    /// terminate: the response was cut short.
    pub truncated: bool,
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Parse a response. `None` when the bytes are not an HTTP/1.1 response head.
#[must_use]
pub fn parse_response(bytes: &[u8]) -> Option<Response> {
    let head_end = find(bytes, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let mut parts = status_line.splitn(3, ' ');
    if parts.next()? != "HTTP/1.1" {
        return None;
    }
    let status: u16 = parts.next()?.parse().ok()?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let raw = &bytes[head_end + 4..];
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.to_ascii_lowercase())
    };
    let (body, truncated) = if get("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        dechunk(raw)
    } else if let Some(len) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
        (raw[..raw.len().min(len)].to_vec(), raw.len() < len)
    } else {
        (raw.to_vec(), false)
    };
    Some(Response {
        status,
        headers,
        body,
        truncated,
    })
}

/// Decode chunked framing; also returns the individual chunk payloads' concatenation.
fn dechunk(raw: &[u8]) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    let mut pos = 0;
    loop {
        let Some(end) = find(&raw[pos..], b"\r\n") else {
            return (body, true);
        };
        let Ok(line) = std::str::from_utf8(&raw[pos..pos + end]) else {
            return (body, true);
        };
        let Ok(size) = usize::from_str_radix(line.trim(), 16) else {
            return (body, true);
        };
        let start = pos + end + 2;
        if size == 0 {
            return (body, false);
        }
        if raw.len() < start + size + 2 {
            return (body, true);
        }
        body.extend_from_slice(&raw[start..start + size]);
        pos = start + size + 2;
    }
}
