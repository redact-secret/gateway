use redact_secret_gateway::admission::MemoryReservation;
use redact_secret_gateway::boundary::SanitizedRequest;
use redact_secret_gateway::config::RouteId;
use redact_secret_gateway::protocol::Protocol;

// The constructor is visible only inside `boundary`.
fn forge(memory: MemoryReservation) -> SanitizedRequest {
    SanitizedRequest::new(Vec::new(), Protocol::ChatCompletionsText, RouteId::new("r"), memory)
}

fn main() {}
