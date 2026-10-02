use redact_secret_gateway::boundary::{SanitizedRequest, approve};
use redact_secret_gateway::config::RouteId;
use redact_secret_gateway::protocol::ValidatedRequest;

// Approval needs the completeness proof, not bytes: an original or partially processed body
// cannot be passed in its place.
fn bypass(validated: ValidatedRequest, original_body: Vec<u8>) -> SanitizedRequest {
    approve(validated, original_body, RouteId::new("r")).unwrap()
}

fn main() {}
