use redact_secret_gateway::admission::UpstreamPermit;
use redact_secret_gateway::boundary::SanitizedRequest;
use redact_secret_gateway::transport::Upstream;
use redact_secret_gateway::transport::headers::VettedHeaders;

// The one accepted body input to forwarding is the sealed final type; the credential
// arrives separately as the request-local vetted headers, with an upstream permit.
async fn send(
    upstream: &Upstream,
    request: SanitizedRequest,
    headers: VettedHeaders,
    permit: UpstreamPermit,
) {
    let _ = upstream.forward(request, headers, permit).await;
}

fn main() {}
