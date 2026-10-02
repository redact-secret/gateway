use redact_secret_gateway::admission::{StreamPermit, UpstreamPermit};
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

// Streaming is not a second door to the network: raw bytes cannot be forwarded as a stream.
async fn send(
    upstream: &Upstream,
    body: Vec<u8>,
    headers: VettedHeaders,
    permit: UpstreamPermit,
    stream: StreamPermit,
) {
    let _ = upstream.forward_stream(body, headers, permit, stream).await;
}

fn main() {}
