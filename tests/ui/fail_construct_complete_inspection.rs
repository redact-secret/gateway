use redact_secret_gateway::core_bridge::{CompleteInspection, InspectionSummary};

// The completeness proof has private fields: only `RequestScope::finish` can mint it, so
// nothing outside the core bridge can claim a request was completely inspected.
fn forge() -> CompleteInspection {
    CompleteInspection {
        output: Vec::new(),
        summary: InspectionSummary::default(),
    }
}

fn main() {}
