use redact_secret_gateway::boundary::SanitizedRequest;

// The final request cannot be mutated outside `boundary`: its body field is private.
fn tamper(request: &mut SanitizedRequest) {
    request.body.push(0);
}

fn main() {}
