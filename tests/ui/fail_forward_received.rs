use redact_secret_gateway::admission::{ReceivedRequest, UpstreamPermit};
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

async fn send(
    upstream: &Upstream,
    request: ReceivedRequest,
    headers: VettedHeaders,
    permit: UpstreamPermit,
) {
    let _ = upstream.forward(request, headers, permit).await;
}

fn main() {}
