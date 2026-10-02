use redact_secret_gateway::transport::destination::Origin;

// Plain-HTTP / non-reviewed test origins are cfg(test) items: absent from the library.
fn main() {
    let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let _ = Origin::for_test_http(addr);
}
