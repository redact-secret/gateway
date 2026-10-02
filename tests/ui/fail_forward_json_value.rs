use redact_secret_gateway::admission::UpstreamPermit;
use redact_secret_gateway::protocol::json::Json;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

async fn send(
    upstream: &Upstream,
    document: Json,
    headers: VettedHeaders,
    permit: UpstreamPermit,
) {
    let _ = upstream.forward(document, headers, permit).await;
}

fn main() {}
