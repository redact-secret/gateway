use redact_secret_gateway::transport::Upstream;

// A destination is selected by RouteId only; a caller-supplied URL string has no path in.
fn pick(upstream: &Upstream, caller_url: &str) {
    let _ = upstream.post(caller_url);
}

fn main() {}
