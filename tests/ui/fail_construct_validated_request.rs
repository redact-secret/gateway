use redact_secret_gateway::protocol::ValidatedRequest;

// A validated request can only come out of the strict parse and classification step; its
// fields are private, so a caller cannot assemble one around unchecked data.
fn forge() -> ValidatedRequest {
    ValidatedRequest {
        request: todo!(),
        memory: todo!(),
        _receipt: todo!(),
    }
}

fn main() {}
