use redact_secret_gateway::admission::MemoryReservation;
use redact_secret_gateway::boundary::SanitizedRequest;
use redact_secret_gateway::config::RouteId;

// The constructor is visible only inside `boundary`.
fn forge(memory: MemoryReservation) -> SanitizedRequest {
    SanitizedRequest::new(Vec::new(), RouteId::new("r"), memory)
}

fn main() {}
