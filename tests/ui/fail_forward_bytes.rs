use redact_secret_gateway::admission::UpstreamPermit;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

async fn send(upstream: &Upstream, body: Vec<u8>, headers: VettedHeaders, permit: UpstreamPermit) {
    let _ = upstream.forward(body, headers, permit).await;
}

fn main() {}
