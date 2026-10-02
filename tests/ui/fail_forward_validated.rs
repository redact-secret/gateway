use redact_secret_gateway::admission::UpstreamPermit;
use redact_secret_gateway::protocol::ValidatedRequest;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

async fn send(
    upstream: &Upstream,
    request: ValidatedRequest,
    headers: VettedHeaders,
    permit: UpstreamPermit,
) {
    let _ = upstream.forward(request, headers, permit).await;
}

fn main() {}
