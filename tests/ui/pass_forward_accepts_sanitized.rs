use redact_secret_gateway::boundary::SanitizedRequest;
use redact_secret_gateway::transport::Upstream;

// The one accepted input to forwarding is the sealed final type.
async fn send(upstream: &Upstream, request: SanitizedRequest) {
    let _ = upstream.forward(request).await;
}

fn main() {}
