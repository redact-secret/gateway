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
//! Memory accounting (#58): `--memory` runs, for every shape and phase, a fresh child
//! process that retains N stage products and reports resident growth per request next to
//! the reservation formula (`--memory-phase <shape> <phase> <bytes> <n>` is the child).
//!
//! ```text
//! cargo run --locked --release --example perf_workloads -- --memory
//! ```
//!
//! Set `GATEWAY_COMMIT` to record the source commit in the header. This example is test
//! support: it is not part of the shipped binary.
#![forbid(unsafe_code)]

#[path = "../tests/support/mod.rs"]
mod support;

use support::workloads::{ALL_SHAPES, Plan, Shape, memory_phase, run_all};

/// Sizes for the generic shapes; the Alpha 2 shapes ignore it or use it as a hint.
fn size_for(shape: Shape) -> usize {
    match shape {
        Shape::NoFindings | Shape::ManyFindings => 16 * 1024,
        Shape::LargeInput => 500_000,
        Shape::ToolHistory => 600_000,
        Shape::ToolDefs => 700_000,
        Shape::Metadata | Shape::NodeDense => 0,
    }
}

fn parse_shape(name: &str) -> Option<Shape> {
    ALL_SHAPES.into_iter().find(|s| s.name() == name)
}

fn memory_parent(smoke: bool) -> std::process::ExitCode {
    let Ok(exe) = std::env::current_exe() else {
        return std::process::ExitCode::FAILURE;
    };
    println!(
        "{}",
        serde_json::json!({
            "kind": "header", "tool": "perf_workloads --memory (retention accounting)",
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        })
    );
    for shape in ALL_SHAPES {
        for phase in ["body", "parsed", "approved"] {
            let size = if smoke {
                size_for(shape).min(64 * 1024)
            } else {
                size_for(shape)
            };
            let n = if smoke { 8 } else { 96 };
            let out = std::process::Command::new(&exe)
                .args([
                    "--memory-phase",
                    shape.name(),
                    phase,
                    &size.to_string(),
                    &n.to_string(),
                ])
                .output();
            if let Ok(out) = out {
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, shape, phase, size, n] = args.as_slice()
        && flag == "--memory-phase"
        && let (Some(shape), Ok(size), Ok(n)) = (
            parse_shape(shape),
            size.parse::<usize>(),
            n.parse::<usize>(),
        )
    {
        println!("{}", memory_phase(shape, size, phase, n).await);
        return std::process::ExitCode::SUCCESS;
    }
    let mut smoke = false;
    let mut memory = false;
    for arg in args {
        if arg == "--smoke" {
            smoke = true;
        } else if arg == "--memory" {
            memory = true;
        } else {
            // Never echo arguments.
            eprintln!("usage: perf_workloads [--smoke]");
            return std::process::ExitCode::from(2);
        }
    }
    if memory {
        return memory_parent(smoke);
    }
    let plan = if smoke { Plan::smoke() } else { Plan::local() };
    let commit = std::env::var("GATEWAY_COMMIT").ok();
    for line in run_all(plan, commit.as_deref()).await {
        println!("{line}");
    }
    std::process::ExitCode::SUCCESS
}
