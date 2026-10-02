use redact_secret_gateway::admission::{InspectionPermit, StreamPermit, UpstreamPermit};

// A permit cannot be duplicated, so one reservation cannot be spent twice or kept after
// the resource it guards has ended.
fn copy_upstream(p: &UpstreamPermit) -> UpstreamPermit {
    p.clone()
}

fn copy_inspection(p: &InspectionPermit) -> InspectionPermit {
    p.clone()
}

fn copy_stream(p: &StreamPermit) -> StreamPermit {
    p.clone()
}

fn main() {}
