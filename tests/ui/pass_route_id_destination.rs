use redact_secret_gateway::config::{Provider, RouteId, UpstreamAuthority};
use redact_secret_gateway::transport::Upstream;

// The reviewed path: authority -> client -> destination by RouteId.
fn main() {
    let authority = UpstreamAuthority::new(Provider::OpenAi);
    let upstream = Upstream::new(Some(&authority)).unwrap();
    let route = RouteId::new("openai.chat_completions");
    let _ = upstream.destination(&route).unwrap().path();
    let _ = upstream.post(&route).unwrap();
}
