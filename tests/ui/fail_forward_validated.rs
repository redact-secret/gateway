use redact_secret_gateway::protocol::ValidatedRequest;
use redact_secret_gateway::transport::Upstream;

async fn send(upstream: &Upstream, request: ValidatedRequest) {
    let _ = upstream.forward(request).await;
}

fn main() {}
