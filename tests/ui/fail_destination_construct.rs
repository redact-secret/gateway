use redact_secret_gateway::transport::destination::{Destination, Origin, RouteMethod};

// Destinations are built only from reviewed provider profiles, never from caller data.
fn main() {
    let origin = Origin::parse("https://api.openai.com").unwrap();
    let _ = Destination::new(origin, "/anything", RouteMethod::Post);
}
