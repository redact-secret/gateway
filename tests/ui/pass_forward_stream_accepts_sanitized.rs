use redact_secret_gateway::admission::{StreamPermit, UpstreamPermit};
use redact_secret_gateway::boundary::SanitizedRequest;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

// The streaming entry point accepts the same sealed final type as `forward`, the same
// request-local credential, and the upstream then stream permits (ADR 0003 order).
async fn send(
    upstream: &Upstream,
    request: SanitizedRequest,
    headers: VettedHeaders,
    permit: UpstreamPermit,
    stream: StreamPermit,
) {
    let _ = upstream.forward_stream(request, headers, permit, stream).await;
}

fn main() {}
