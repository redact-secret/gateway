use redact_secret_gateway::protocol::json::Json;
use redact_secret_gateway::transport::Upstream;

async fn send(upstream: &Upstream, document: Json) {
    let _ = upstream.forward(document).await;
}

fn main() {}
