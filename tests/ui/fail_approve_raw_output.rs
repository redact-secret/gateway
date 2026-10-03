use redact_secret_gateway::boundary::{ProtocolRoute, SanitizedRequest, approve};
use redact_secret_gateway::config::RouteId;
use redact_secret_gateway::protocol::{Protocol, ValidatedRequest};

// Approval needs the completeness proof, not bytes: an original or partially processed body
// cannot be passed in its place.
fn bypass(validated: ValidatedRequest, original_body: Vec<u8>) -> SanitizedRequest {
    let route = ProtocolRoute::new(Protocol::ChatCompletionsText, RouteId::new("r"));
    approve(validated, original_body, route).unwrap()
}

fn main() {}
