use redact_secret_gateway::admission::UpstreamPermit;

// A permit proves capacity was reserved. Its field is private, so nothing outside the
// admission module can mint one (or return one early by fabricating a replacement).
fn forge() -> UpstreamPermit {
    UpstreamPermit { _permit: todo!() }
}

fn main() {}
