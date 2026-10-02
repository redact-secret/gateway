use redact_secret_gateway::transport::Upstream;

async fn send(upstream: &Upstream, body: Vec<u8>) {
    let _ = upstream.forward(body).await;
}

fn main() {}
