use redact_secret_gateway::boundary::SanitizedRequest;

// `SanitizedRequest` has no `Default`: an empty or placeholder final request cannot be
// conjured outside `boundary`.
fn forge() -> SanitizedRequest {
    SanitizedRequest::default()
}

fn main() {}
