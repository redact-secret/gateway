use redact_secret_gateway::transport::credential::ProviderCredential;

// The provider credential is request-local transport authority: it cannot be cloned
// (kept for replay or shared state), compared (no equality oracle), displayed, or
// defaulted.
fn duplicate(c: &ProviderCredential) -> ProviderCredential {
    c.clone()
}

fn equal(a: &ProviderCredential, b: &ProviderCredential) -> bool {
    a == b
}

fn show(c: &ProviderCredential) -> String {
    format!("{c}")
}

fn fresh() -> ProviderCredential {
    ProviderCredential::default()
}

fn main() {}
