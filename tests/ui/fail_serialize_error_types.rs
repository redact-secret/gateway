use redact_secret_gateway::admission::AdmissionError;
use redact_secret_gateway::boundary::BoundaryError;
use redact_secret_gateway::chat_route::Reject;
use redact_secret_gateway::core_bridge::CoreBridgeError;
use redact_secret_gateway::server::StartupError;
use redact_secret_gateway::transport::TransportError;
use redact_secret_gateway::transport::credential::ProviderCredential;
use redact_secret_gateway::transport::headers::VettedHeaders;

// Diagnostics are text from a fixed vocabulary, never structured dumps: none of the error
// or credential-bearing types can be serialized.
fn needs_serialize<T: serde::Serialize>() {}

fn check() {
    needs_serialize::<TransportError>();
    needs_serialize::<Reject>();
    needs_serialize::<BoundaryError>();
    needs_serialize::<CoreBridgeError>();
    needs_serialize::<AdmissionError>();
    needs_serialize::<StartupError>();
    needs_serialize::<ProviderCredential>();
    needs_serialize::<VettedHeaders>();
}

fn main() {}
