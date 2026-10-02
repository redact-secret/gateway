use redact_secret_gateway::boundary::SanitizedRequest;

// No Clone: an approved body cannot be duplicated and mutated elsewhere.
fn duplicate(request: &SanitizedRequest) -> SanitizedRequest {
    request.clone()
}

fn main() {}
