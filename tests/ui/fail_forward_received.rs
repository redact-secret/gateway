use redact_secret_gateway::admission::ReceivedRequest;
use redact_secret_gateway::transport::Upstream;

async fn send(upstream: &Upstream, request: ReceivedRequest) {
    let _ = upstream.forward(request).await;
}

fn main() {}
