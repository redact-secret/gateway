use redact_secret_gateway::boundary::SanitizedRequest;

// The public accessor returns a shared slice, so the body cannot be edited through it.
fn tamper(request: &mut SanitizedRequest) {
    request.body()[0] = 1;
}

fn main() {}
