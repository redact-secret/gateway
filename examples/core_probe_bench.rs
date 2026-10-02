//! Reproducible core probe: setup cost, steady-state latency, and inline versus offload
//! scheduling (issue #5, ADR 0004, ADR 0008).
//!
//! Run (release is required for meaningful numbers; the example refuses debug builds unless
//! `--allow-debug` is given):
//!
//! ```text
//! cargo run --locked --release --example core_probe_bench            # full run
//! cargo run --locked --release --example core_probe_bench -- --quick # smoke
//! ```
//!
//! Inputs are generated, synthetic, and deterministic. Output is timings and counts only,
//! never payload text. Results are a baseline for choosing a scheduling strategy on the
//! machine they were measured on; they are not service-level performance claims.
//!
//! Not part of the shipped binary: examples are built only by `--examples` / `--all-targets`.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "benchmark harness over generated synthetic input, not production code"
)]

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::{Duration, Instant};

use redact_secret_gateway::admission::{Admission, CapacityPlan};
use redact_secret_gateway::core_bridge::pool::InspectionPool;
use redact_secret_gateway::core_bridge::{
    Inspector, InspectorSpec, PINNED_CORE_VERSION, RequestScope,
};

const WORDS: [&str; 32] = [
    "alpha", "bridge", "cobalt", "delta", "ember", "falcon", "garden", "harbor", "island",
    "jungle", "kernel", "lantern", "meadow", "nectar", "orbit", "pepper", "quartz", "river",
    "summit", "timber", "umbra", "velvet", "willow", "xenon", "yonder", "zephyr", "amber",
    "basalt", "canyon", "dune", "forest", "glacier",
];

/// Deterministic benign prose of about `bytes` bytes (no credential-shaped strings).
fn prose(bytes: usize) -> String {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = String::with_capacity(bytes + 16);
    while out.len() < bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push_str(WORDS[((state >> 33) % 32) as usize]);
        out.push(if (state >> 20).is_multiple_of(11) {
            '\n'
        } else {
            ' '
        });
    }
    out
}

/// Prose with one distinct synthetic revoked-style token about every `every` bytes.
fn dense(bytes: usize, every: usize) -> (String, usize) {
    let filler = prose(every);
    let mut out = String::with_capacity(bytes + 64);
    let mut n = 0_u64;
    while out.len() < bytes {
        out.push_str("key=ghp_SYNTHETICREVOKED");
        out.push_str(&format!("{n:020}"));
        out.push(' ');
        out.push_str(&filler);
        n += 1;
    }
    (out, n as usize)
}

#[derive(Clone, Copy)]
struct Stats {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    n: usize,
}

fn stats(mut micros: Vec<f64>) -> Stats {
    micros.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = micros.len();
    let at = |q: f64| -> f64 {
        let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
        micros[rank - 1]
    };
    Stats {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: micros[n - 1],
        n,
    }
}

fn row(label: &str, s: Stats) {
    println!(
        "| {label} | {} | {:.1} | {:.1} | {:.1} | {:.1} |",
        s.n, s.p50, s.p95, s.p99, s.max
    );
}

fn header() {
    println!("| case | n | p50 us | p95 us | p99 us | max us |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn spec(profile: &str) -> InspectorSpec {
    // Probe-only bounds, large enough for every generated case. Not recommended defaults.
    InspectorSpec::new(profile, &[], 64 << 20, 50_000).expect("spec")
}

fn setup_costs(iterations: usize) {
    println!("\n### Core setup (registry construction), single thread\n");
    header();
    for (label, spec) in [("full", spec("full")), ("common", spec("common"))] {
        let first = Instant::now();
        let _ = Inspector::new(&spec).expect("inspector");
        println!(
            "| {label} first build in process (includes one-time prefilter) | 1 | {:.1} | - | - | - |",
            micros(first.elapsed())
        );
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let t = Instant::now();
            let i = Inspector::new(&spec).expect("inspector");
            samples.push(micros(t.elapsed()));
            drop(i);
        }
        row(&format!("{label} build, steady"), stats(samples));
    }
    let pii = InspectorSpec::new("full", &["pii"], 64 << 20, 50_000).expect("spec");
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        let i = Inspector::new(&pii).expect("inspector");
        samples.push(micros(t.elapsed()));
        drop(i);
    }
    row("full + pii build, steady", stats(samples));
}

fn steady_state(quick: bool) {
    println!("\n### Steady-state inspection, one thread, reused inspector (full profile)\n");
    header();
    let spec = spec("full");
    let inspector = Inspector::new(&spec).expect("inspector");
    // (label, input, iterations)
    let scale = |n: usize| if quick { (n / 10).max(3) } else { n };
    let mut cases: Vec<(String, String, usize)> = vec![
        ("no findings 1 KiB".into(), prose(1 << 10), scale(2000)),
        ("no findings 64 KiB".into(), prose(64 << 10), scale(300)),
        ("no findings 1 MiB".into(), prose(1 << 20), scale(40)),
        ("no findings 8 MiB".into(), prose(8 << 20), scale(10)),
    ];
    let (d1, n1) = dense(64 << 10, 40);
    cases.push((
        format!("many findings 64 KiB ({n1} findings)"),
        d1,
        scale(100),
    ));
    let (d2, n2) = dense(1 << 20, 40);
    cases.push((
        format!("many findings 1 MiB ({n2} findings)"),
        d2,
        scale(10),
    ));
    for (label, input, iterations) in cases {
        // Warm-up, not recorded.
        for _ in 0..2 {
            let mut scope = RequestScope::new(&spec);
            let _ = inspector.inspect_text(&mut scope, &input).expect("ok");
        }
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let mut scope = RequestScope::new(&spec);
            let t = Instant::now();
            let out = inspector.inspect_text(&mut scope, &input).expect("ok");
            samples.push(micros(t.elapsed()));
            std::hint::black_box(out);
        }
        row(&label, stats(samples));
    }
}

struct Report {
    latency: Stats,
    tick_lateness: Stats,
    wall: Duration,
    requests: usize,
}

/// Closed loop: `concurrency` tasks each issue `requests / concurrency` requests of `input`
/// on a current-thread runtime, while a 1 ms ticker measures how late the reactor wakes it.
/// `workers == 0` runs the core call inline on the reactor thread; otherwise it is offloaded
/// to a dedicated pool.
fn scheduling(input: &str, requests: usize, concurrency: usize, workers: usize) -> Report {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let spec = Rc::new(spec("full"));
    let big = NonZeroU32::new(u32::MAX / 2).unwrap();
    let admission = Rc::new(Admission::new(&CapacityPlan::new(big, big, big, big, big)));
    let pool = Rc::new((workers > 0).then(|| {
        InspectionPool::start(
            &spec,
            NonZeroUsize::new(workers).unwrap(),
            NonZeroUsize::new(concurrency.max(1)).unwrap(),
        )
        .expect("pool")
    }));
    let inline = Rc::new((workers == 0).then(|| Inspector::new(&spec).expect("inspector")));
    let input: Rc<str> = Rc::from(input);
    let per_task = (requests / concurrency).max(1);

    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let ticks = Rc::new(RefCell::new(Vec::<f64>::new()));
        let stop = Rc::new(Cell::new(false));
        let ticker = {
            let ticks = Rc::clone(&ticks);
            let stop = Rc::clone(&stop);
            tokio::task::spawn_local(async move {
                let want = Duration::from_millis(1);
                while !stop.get() {
                    let t = Instant::now();
                    tokio::time::sleep(want).await;
                    ticks
                        .borrow_mut()
                        .push(micros(t.elapsed().saturating_sub(want)));
                }
            })
        };
        let latencies = Rc::new(RefCell::new(Vec::<f64>::new()));
        let started = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..concurrency {
            let (spec, admission, pool, inline, input, latencies) = (
                Rc::clone(&spec),
                Rc::clone(&admission),
                Rc::clone(&pool),
                Rc::clone(&inline),
                Rc::clone(&input),
                Rc::clone(&latencies),
            );
            tasks.push(tokio::task::spawn_local(async move {
                for _ in 0..per_task {
                    let t = Instant::now();
                    if let Some(pool) = pool.as_ref() {
                        let handle = pool
                            .submit_inspect(
                                admission.try_inspection().expect("permit"),
                                admission.try_reserve_memory(1).expect("memory"),
                                RequestScope::new(&spec),
                                input.to_string(),
                            )
                            .expect("submit");
                        let (scope, out) = handle.await.expect("done");
                        std::hint::black_box((scope, out.expect("ok")));
                    } else if let Some(inspector) = inline.as_ref() {
                        // Runs on the reactor thread: everything else waits, ticker included.
                        let mut scope = RequestScope::new(&spec);
                        let out = inspector.inspect_text(&mut scope, &input).expect("ok");
                        std::hint::black_box(out);
                        tokio::task::yield_now().await;
                    }
                    latencies.borrow_mut().push(micros(t.elapsed()));
                }
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }
        let wall = started.elapsed();
        stop.set(true);
        let _ = ticker.await;
        let tick_samples = ticks.borrow().clone();
        let latency_samples = latencies.borrow().clone();
        Report {
            latency: stats(latency_samples),
            tick_lateness: stats(if tick_samples.is_empty() {
                vec![0.0]
            } else {
                tick_samples
            }),
            wall,
            requests: per_task * concurrency,
        }
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    if cfg!(debug_assertions) && !args.iter().any(|a| a == "--allow-debug") {
        eprintln!("refusing to measure a debug build; use --release (or --allow-debug)");
        std::process::exit(2);
    }
    println!("# core probe bench\n");
    println!(
        "- gateway: redact-secret-gateway {}",
        env!("CARGO_PKG_VERSION")
    );
    println!("- core pin: redact-secret =={PINNED_CORE_VERSION}");
    println!(
        "- os/arch: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "- logical cpus: {}",
        std::thread::available_parallelism().map_or(0, NonZeroUsize::get)
    );
    println!(
        "- profile: full (credentials), no PII unless stated; release={}",
        !cfg!(debug_assertions)
    );
    println!("- mode: {}", if quick { "quick (smoke)" } else { "full" });

    setup_costs(if quick { 50 } else { 2000 });
    steady_state(quick);

    println!("\n### Inline on the reactor thread versus dedicated worker pool\n");
    println!("Current-thread runtime plus a 1 ms ticker. Lateness is how long past 1 ms the");
    println!("ticker woke up: the reactor stall a request imposes on everything else.\n");
    println!(
        "| input | mode | requests | concurrency | wall ms | req/s | latency p50 us | p95 us | p99 us | tick lateness p50 us | p99 us | max us |"
    );
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let cases: Vec<(&str, String, usize)> = vec![
        (
            "1 KiB no findings",
            prose(1 << 10),
            if quick { 50 } else { 2000 },
        ),
        (
            "64 KiB no findings",
            prose(64 << 10),
            if quick { 20 } else { 400 },
        ),
        (
            "1 MiB no findings",
            prose(1 << 20),
            if quick { 5 } else { 60 },
        ),
        (
            "64 KiB many findings",
            dense(64 << 10, 40).0,
            if quick { 10 } else { 200 },
        ),
    ];
    let cpus = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    for (label, input, requests) in &cases {
        for concurrency in [8_usize] {
            for workers in [0_usize, 1, 2, 4, cpus] {
                let r = scheduling(input, *requests, concurrency, workers);
                let mode = if workers == 0 {
                    "inline".to_owned()
                } else {
                    format!("pool({workers})")
                };
                println!(
                    "| {label} | {mode} | {} | {concurrency} | {:.1} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                    r.requests,
                    r.wall.as_secs_f64() * 1e3,
                    r.requests as f64 / r.wall.as_secs_f64(),
                    r.latency.p50,
                    r.latency.p95,
                    r.latency.p99,
                    r.tick_lateness.p50,
                    r.tick_lateness.p99,
                    r.tick_lateness.max,
                );
            }
        }
    }
}
