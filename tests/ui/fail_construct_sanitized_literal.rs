use redact_secret_gateway::boundary::SanitizedRequest;

// Fields are private: a struct literal outside `boundary` cannot forge the final type.
fn forge() -> SanitizedRequest {
    SanitizedRequest {
        body: Vec::new(),
    }
}

fn main() {}
