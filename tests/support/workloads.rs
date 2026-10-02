//! Synthetic performance workload scaffolding (ADR 0008, issue #6).
//!
//! This is a *measurement tool*, not evidence of performance. It generates synthetic
//! payload shapes, runs the phases the skeleton actually implements, and records coarse
//! timing percentiles and resident memory. It makes no performance claim, and no number
//! it prints may enter configuration defaults without the recorded pins ADR 0008 requires.
//!
//! Shapes: no-findings, many-findings, large input, concurrent requests, slow SSE
//! consumer. "Findings" here means payload *shape* (many credential-looking synthetic
//! tokens); core inspection is not run (that is #5's probe), so detection work is not
//! being measured and the phase is reported as not measured.
//!
//! Output hygiene: records carry workload and phase names, sizes, counts, and timings.
//! They never carry payload bytes, snippets, header values, or credential labels. Tests
//! scan the rendered report with the leakage scanner.

use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redact_secret_gateway::admission::{Admission, AdmissionError, CapacityPlan};
use redact_secret_gateway::protocol::{self, Protocol};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::fake_upstream::{Behavior, FakeUpstream, SseFragment, SseFraming};
use super::raw_http;

/// ADR 0008 phases the skeleton cannot measure yet.
pub const NOT_MEASURED: [&str; 3] = [
    "core_inspection_and_transformation (needs core probe, #5)",
    "serialization (needs forwarding path, #18/#19)",
    "gateway_total_excluding_upstream (needs forwarding path, #18/#19)",
];

/// Payload shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    NoFindings,
    ManyFindings,
    LargeInput,
}

impl Shape {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NoFindings => "no_findings",
            Self::ManyFindings => "many_findings",
            Self::LargeInput => "large_input",
        }
    }
}

/// Build a Chat-Completions-shaped JSON body of roughly `target_bytes` bytes. Content is
/// invented filler; `ManyFindings` repeats unmistakably synthetic credential-shaped
/// tokens.
#[must_use]
pub fn payload(shape: Shape, target_bytes: usize) -> Vec<u8> {
    let mut text = String::with_capacity(target_bytes);
    let mut n: u64 = 0;
    while text.len() < target_bytes {
        match shape {
            Shape::NoFindings | Shape::LargeInput => {
                text.push_str("synthetic filler sentence number ");
                let _ = write!(text, "{n} ");
            }
            Shape::ManyFindings => {
                let _ = write!(text, "SYNTH-FINDING-{n:06}-NOT-A-CREDENTIAL ");
            }
        }
        n += 1;
    }
    let doc = json!({
        "model": "synthetic-model",
        "messages": [{"role": "user", "content": text}],
    });
    serde_json::to_vec(&doc).unwrap_or_default()
}

/// Sorted nanosecond samples with percentile lookup.
#[derive(Debug, Default)]
pub struct Samples(Vec<u128>);

impl Samples {
    pub fn push(&mut self, d: Duration) {
        self.0.push(d.as_nanos());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Nearest-rank percentile in nanoseconds; 0 when empty.
    #[must_use]
    pub fn percentile(&self, p: f64) -> u128 {
        if self.0.is_empty() {
            return 0;
        }
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }
}

/// Resident memory, with the source so readers know whether it is a true peak.
#[derive(Clone, Copy, Debug)]
pub struct Rss {
    pub kb: Option<u64>,
    /// `vmhwm` is a process-lifetime peak (Linux). `ps_current` is a point-in-time sample.
    pub source: &'static str,
}

/// Process resident memory in KiB. No unsafe: `/proc` on Linux, `ps` elsewhere.
#[must_use]
pub fn rss() -> Rss {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        let kb = status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok());
        return Rss {
            kb,
            source: "vmhwm",
        };
    }
    let kb = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok());
    Rss {
        kb,
        source: "ps_current",
    }
}

/// One measured result. Contains no payload content.
#[derive(Debug)]
pub struct Record {
    pub workload: &'static str,
    pub phase: &'static str,
    pub input_bytes: usize,
    pub iterations: usize,
    pub concurrency: usize,
    pub overload_rejections: usize,
    pub samples: Samples,
    pub rss: Rss,
}

impl Record {
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "workload": self.workload,
            "phase": self.phase,
            "input_bytes": self.input_bytes,
            "iterations": self.iterations,
            "concurrency": self.concurrency,
            "overload_rejections": self.overload_rejections,
            "p50_ns": self.samples.percentile(50.0).to_string(),
            "p95_ns": self.samples.percentile(95.0).to_string(),
            "p99_ns": self.samples.percentile(99.0).to_string(),
            "rss_kb": self.rss.kb,
            "rss_source": self.rss.source,
        })
    }
}

/// Run-level pins recorded with every report (ADR 0008: commit, core pin, platform).
#[must_use]
pub fn header(source_commit: Option<&str>) -> Value {
    json!({
        "kind": "header",
        "tool": "perf_workloads scaffold (no performance claim)",
        "gateway_version": env!("CARGO_PKG_VERSION"),
        "core_pin": redact_secret_gateway::core_bridge::PINNED_CORE_VERSION,
        "source_commit": source_commit,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "not_measured": NOT_MEASURED,
    })
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN)
}

fn units_for(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// One bounded receipt + strict parse, as the skeleton implements it: reserve first, take
/// the body, `protocol::validate` (parse, then reject as unsupported).
fn receipt_and_parse(admission: &Admission, body: &[u8]) -> Result<(), AdmissionError> {
    let ticket = admission.begin_receipt(units_for(body.len()))?;
    let received = ticket.complete(body.to_vec())?;
    // Unsupported is the expected skeleton outcome; the cost measured is the parse.
    let _ = protocol::validate(received, Protocol::ChatCompletionsText);
    Ok(())
}

/// Sequential receipt + parse of one payload shape.
#[must_use]
pub fn run_parse(shape: Shape, size: usize, iterations: usize) -> Record {
    let body = payload(shape, size);
    let units = units_for(body.len());
    let admission = Admission::new(&CapacityPlan::new(nz(1), nz(units), nz(1), nz(1), nz(1)));
    let mut samples = Samples::default();
    let mut overload = 0;
    for _ in 0..iterations {
        let t = Instant::now();
        if receipt_and_parse(&admission, &body).is_err() {
            overload += 1;
        }
        samples.push(t.elapsed());
    }
    Record {
        workload: shape.name(),
        phase: "receipt_and_parsing",
        input_bytes: body.len(),
        iterations,
        concurrency: 1,
        overload_rejections: overload,
        samples,
        rss: rss(),
    }
}

/// Concurrent receipt + parse with a memory budget that admits only `admitted` bodies at
/// once. Excess requests are rejected immediately (bounded overload, no queue).
#[must_use]
pub fn run_concurrent(
    shape: Shape,
    size: usize,
    threads: usize,
    per_thread: usize,
    admitted: u32,
) -> Record {
    let body = Arc::new(payload(shape, size));
    let units = units_for(body.len());
    let admission = Arc::new(Admission::new(&CapacityPlan::new(
        nz(admitted),
        nz(units.saturating_mul(admitted)),
        nz(1),
        nz(1),
        nz(1),
    )));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let admission = Arc::clone(&admission);
            let body = Arc::clone(&body);
            std::thread::spawn(move || {
                let mut local = Samples::default();
                let mut overload = 0;
                for _ in 0..per_thread {
                    let t = Instant::now();
                    if receipt_and_parse(&admission, &body).is_err() {
                        overload += 1;
                    }
                    local.push(t.elapsed());
                }
                (local, overload)
            })
        })
        .collect();
    let mut samples = Samples::default();
    let mut overload = 0;
    for h in handles {
        if let Ok((local, o)) = h.join() {
            samples.0.extend(local.0);
            overload += o;
        }
    }
    Record {
        workload: "concurrent",
        phase: "receipt_and_parsing",
        input_bytes: body.len(),
        iterations: threads * per_thread,
        concurrency: threads,
        overload_rejections: overload,
        samples,
        rss: rss(),
    }
}

/// A slow SSE consumer: the fake upstream streams `fragments` pieces; the reader sleeps
/// `consumer_delay` between reads while holding a stream permit. Records time to first
/// byte and total stream time as two samples sets in one record per phase.
pub async fn run_slow_sse(fragments: usize, consumer_delay: Duration) -> [Record; 2] {
    let frags: Vec<SseFragment> = (0..fragments)
        .map(|i| SseFragment::now(format!("data: synthetic-event-{i}\n\n").into_bytes()))
        .collect();
    let upstream = FakeUpstream::start(Behavior::Sse {
        fragments: frags,
        framing: SseFraming::Chunked,
        finish: true,
    })
    .await;
    let admission = Admission::new(&CapacityPlan::new(nz(1), nz(1), nz(1), nz(1), nz(1)));
    let permit = admission.try_stream().ok();

    let started = Instant::now();
    let mut first_byte = Samples::default();
    let mut total = Samples::default();
    let mut received = 0_usize;
    if let Ok(mut stream) = tokio::net::TcpStream::connect(upstream.addr()).await {
        let request = raw_http::post("/v1/chat/completions", &[], b"{}");
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &request).await;
        let mut buf = [0_u8; 64];
        let mut saw_first = false;
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    received += n;
                    if !saw_first {
                        first_byte.push(started.elapsed());
                        saw_first = true;
                    }
                }
            }
            tokio::time::sleep(consumer_delay).await;
        }
    }
    total.push(started.elapsed());
    drop(permit);
    let rss = rss();
    [
        Record {
            workload: "slow_sse",
            phase: "upstream_first_response",
            input_bytes: 0,
            iterations: 1,
            concurrency: 1,
            overload_rejections: 0,
            samples: first_byte,
            rss,
        },
        Record {
            workload: "slow_sse",
            phase: "stream_relay",
            input_bytes: received,
            iterations: 1,
            concurrency: 1,
            overload_rejections: 0,
            samples: total,
            rss,
        },
    ]
}

/// Sizes for one run.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub small: usize,
    pub large: usize,
    pub iterations: usize,
    pub threads: usize,
    pub sse_fragments: usize,
    pub sse_delay: Duration,
}

impl Plan {
    /// Tiny sizes for CI smoke runs (seconds, small memory).
    #[must_use]
    pub fn smoke() -> Self {
        Self {
            small: 4 * 1024,
            large: 256 * 1024,
            iterations: 5,
            threads: 4,
            sse_fragments: 5,
            sse_delay: Duration::from_millis(2),
        }
    }

    /// Larger default for local measurement runs.
    #[must_use]
    pub fn local() -> Self {
        Self {
            small: 16 * 1024,
            large: 8 * 1024 * 1024,
            iterations: 50,
            threads: 8,
            sse_fragments: 50,
            sse_delay: Duration::from_millis(10),
        }
    }
}

/// Run every workload and return JSON records (header first).
pub async fn run_all(plan: Plan, source_commit: Option<&str>) -> Vec<Value> {
    let mut out = vec![header(source_commit)];
    let mut records = vec![
        run_parse(Shape::NoFindings, plan.small, plan.iterations),
        run_parse(Shape::ManyFindings, plan.small, plan.iterations),
        run_parse(Shape::LargeInput, plan.large, plan.iterations.min(10)),
        run_concurrent(
            Shape::NoFindings,
            plan.small,
            plan.threads,
            plan.iterations,
            2,
        ),
    ];
    records.extend(run_slow_sse(plan.sse_fragments, plan.sse_delay).await);
    out.extend(records.iter().map(Record::to_json));
    out
}
