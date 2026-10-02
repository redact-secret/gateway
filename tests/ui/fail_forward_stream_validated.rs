use redact_secret_gateway::admission::{StreamPermit, UpstreamPermit};
use redact_secret_gateway::protocol::ValidatedRequest;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

// The streaming door takes only the sealed final type: a validated (parsed but not
// inspected) request cannot be streamed to a provider either.
async fn send(
    upstream: &Upstream,
    request: ValidatedRequest,
    headers: VettedHeaders,
    permit: UpstreamPermit,
    stream: StreamPermit,
) {
    let _ = upstream.forward_stream(request, headers, permit, stream).await;
}

fn main() {}
