//! Synthetic performance workload runner (ADR 0008, issue #6). Measurement scaffolding
//! only: it records coarse timing percentiles and resident memory per phase as JSON
//! lines and makes no performance claim. Output never contains payloads or credential
//! labels.
//!
//! ```text
//! cargo run --locked --example perf_workloads -- --smoke   # tiny sizes (CI)
//! cargo run --locked --release --example perf_workloads    # local measurement run
//! ```
//!
//! Set `GATEWAY_COMMIT` to record the source commit in the header. This example is test
//! support: it is not part of the shipped binary.
#![forbid(unsafe_code)]

#[path = "../tests/support/mod.rs"]
mod support;

use support::workloads::{Plan, run_all};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let mut smoke = false;
    for arg in std::env::args().skip(1) {
        if arg == "--smoke" {
            smoke = true;
        } else {
            // Never echo arguments.
            eprintln!("usage: perf_workloads [--smoke]");
            return std::process::ExitCode::from(2);
        }
    }
    let plan = if smoke { Plan::smoke() } else { Plan::local() };
    let commit = std::env::var("GATEWAY_COMMIT").ok();
    for line in run_all(plan, commit.as_deref()).await {
        println!("{line}");
    }
    std::process::ExitCode::SUCCESS
}
