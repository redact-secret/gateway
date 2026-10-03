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

use redact_secret::Profile;
use redact_secret_gateway::admission::{Admission, AdmissionError, CapacityPlan, RequestLimits};
use redact_secret_gateway::boundary::Inspection;
use redact_secret_gateway::boundary::ProtocolRoute;
use redact_secret_gateway::config::{ContentPolicy, RouteId};
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
    /// Alpha 2 (#58): assistant `tool_calls` with JSON-string arguments (nested, with
    /// credential-shaped synthetic tokens) followed by `tool` results, repeated.
    ToolHistory,
    /// Alpha 2: 64 tool definitions with many described properties.
    ToolDefs,
    /// Alpha 2: 16 metadata entries of 512-byte values (half carry a synthetic token).
    Metadata,
    /// Alpha 2: node-dense schemas (large enums) that approach the parsed-node budget with
    /// few bytes, the worst case for the per-node term of the reservation formula.
    NodeDense,
}

/// Every shape, in a stable order.
pub const ALL_SHAPES: [Shape; 7] = [
    Shape::NoFindings,
    Shape::ManyFindings,
    Shape::LargeInput,
    Shape::ToolHistory,
    Shape::ToolDefs,
    Shape::Metadata,
    Shape::NodeDense,
];

impl Shape {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NoFindings => "no_findings",
            Self::ManyFindings => "many_findings",
            Self::LargeInput => "large_input",
            Self::ToolHistory => "tool_history",
            Self::ToolDefs => "tool_defs",
            Self::Metadata => "metadata",
            Self::NodeDense => "node_dense",
        }
    }
}

/// Build a Chat-Completions-shaped JSON body of roughly `target_bytes` bytes. Content is
/// invented filler; `ManyFindings` repeats unmistakably synthetic credential-shaped
/// tokens.
#[must_use]
pub fn payload(shape: Shape, target_bytes: usize) -> Vec<u8> {
    match shape {
        Shape::ToolHistory => return tool_history(target_bytes),
        Shape::ToolDefs => return tool_defs(target_bytes),
        Shape::Metadata => return metadata_heavy(),
        Shape::NodeDense => return node_dense(),
        Shape::NoFindings | Shape::ManyFindings | Shape::LargeInput => {}
    }
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
            _ => break,
        }
        n += 1;
    }
    let doc = json!({
        "model": "synthetic-model",
        "messages": [{"role": "user", "content": text}],
    });
    serde_json::to_vec(&doc).unwrap_or_default()
}

fn filler(n: usize, len: usize) -> String {
    let mut text = String::with_capacity(len + 40);
    let mut i = 0_usize;
    while text.len() < len {
        let _ = write!(text, "synthetic filler {n} {i} ");
        i += 1;
    }
    text.truncate(len);
    text
}

/// A revoked-looking synthetic token (the leak scanner and the core both know the shape).
fn token(n: usize) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn chat(extra: &Value, messages: Value) -> Vec<u8> {
    let mut doc = json!({"model": "synthetic-model", "messages": messages});
    if let (Some(base), Some(add)) = (doc.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            base.insert(k.clone(), v.clone());
        }
    }
    serde_json::to_vec(&doc).unwrap_or_default()
}

/// Tool history: turns of one assistant message with 4 tool calls (arguments nested 5 deep,
/// one synthetic token each) and 4 tool results (one token each), until about `target`.
fn tool_history(target: usize) -> Vec<u8> {
    let turns = 48_usize;
    let per_call = (target / (turns * 8)).max(32);
    let mut messages = vec![json!({"role": "user", "content": "run the lookups"})];
    let mut n = 0_usize;
    for t in 0..turns {
        let mut calls = Vec::new();
        for c in 0..4 {
            let args = json!({
                "query": format!("{} {}", filler(n, per_call), token(n)),
                "filters": {"a": {"b": {"c": {"d": format!("v{n}")}}}},
                "limit": 5,
            });
            calls.push(json!({
                "id": format!("call_{t}_{c}"),
                "type": "function",
                "function": {"name": "lookup_record", "arguments": args.to_string()},
            }));
            n += 1;
        }
        messages.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": calls}));
        for c in 0..4 {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{t}_{c}"),
                "content": format!("{} {}", filler(n, per_call), token(n)),
            }));
            n += 1;
        }
    }
    chat(&json!({}), Value::Array(messages))
}

/// 64 tools, each with 32 described string properties and a `required` list.
fn tool_defs(target: usize) -> Vec<u8> {
    let tools = 64_usize;
    let props = 32_usize;
    let desc = (target / (tools * props)).clamp(16, 1024);
    let mut list = Vec::new();
    for k in 0..tools {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for j in 0..props {
            let name = format!("p_{j}");
            let text = if j % 8 == 0 {
                format!("{} {}", filler(k * props + j, desc), token(k * props + j))
            } else {
                filler(k * props + j, desc)
            };
            properties.insert(name.clone(), json!({"type": "string", "description": text}));
            if j < 4 {
                required.push(Value::String(name));
            }
        }
        list.push(json!({
            "type": "function",
            "function": {
                "name": format!("tool_{k}"),
                "description": filler(k, desc),
                "parameters": {"type": "object", "properties": properties, "required": required},
            },
        }));
    }
    chat(
        &json!({"tools": list}),
        json!([{"role": "user", "content": "use a tool"}]),
    )
}

/// 16 metadata entries, 64-byte-or-shorter keys, 512-byte values, every other value with a token.
fn metadata_heavy() -> Vec<u8> {
    let mut meta = serde_json::Map::new();
    for i in 0..16_usize {
        let mut v = filler(i, 440);
        if i % 2 == 0 {
            v.push(' ');
            v.push_str(&token(i));
        }
        v.truncate(512);
        meta.insert(format!("meta_key_{i:02}"), Value::String(v));
    }
    chat(
        &json!({"metadata": meta}),
        json!([{"role": "user", "content": "hello"}]),
    )
}

/// 5 tools x 64 string properties x 40 short enum labels: about 15k parsed nodes in under
/// 100 KiB, close to `max_nodes` (16,384) with a small body.
fn node_dense() -> Vec<u8> {
    let mut list = Vec::new();
    for k in 0..5_usize {
        let mut properties = serde_json::Map::new();
        for j in 0..64_usize {
            let labels: Vec<String> = (0..40).map(|e| format!("v{e}")).collect();
            properties.insert(format!("p_{j}"), json!({"type": "string", "enum": labels}));
        }
        list.push(json!({
            "type": "function",
            "function": {
                "name": format!("dense_{k}"),
                "parameters": {"type": "object", "properties": properties},
            },
        }));
    }
    chat(
        &json!({"tools": list}),
        json!([{"role": "user", "content": "use a tool"}]),
    )
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

/// Memory accounting by retention (#58): hold `n` copies of one stage's product alive and
/// divide the growth of the process's resident memory by `n`. The caller runs each
/// `(shape, phase)` in a fresh process so freed memory from an earlier phase cannot hide
/// growth. Phases: `body` (the received buffer, a control for the method), `parsed` (the
/// validated typed request with decoded text), `approved` (the sealed transformed output,
/// which is what the request keeps after inspection). Output carries sizes only.
pub async fn memory_phase(shape: Shape, size: usize, phase: &str, n: usize) -> Value {
    let body = payload(shape, size);
    let limits = RequestLimits::provisional();
    let units = limits.reservation_units(body.len());
    let total = units.saturating_mul(u32::try_from(n + 2).unwrap_or(u32::MAX));
    let plan = CapacityPlan::new(nz(u32::MAX >> 1), nz(total), nz(2), nz(1), nz(1));
    let admission = Arc::new(Admission::new(&plan));
    let content = ContentPolicy::new(Profile::Full);
    let Ok(inspection) = Inspection::start(Arc::clone(&admission), &content, &limits, &plan) else {
        return json!({"phase": phase, "error": "inspection_start"});
    };

    // One request through the whole path, dropped, before the baseline: lazy initialization
    // and thread stacks are not the thing being measured.
    let one = |bytes: Vec<u8>| {
        let admission = Arc::clone(&admission);
        async move {
            let ticket = admission
                .begin_body_receipt(bytes.len(), &limits)
                .await
                .ok()?;
            let received = ticket.complete(bytes).ok()?;
            protocol::validate_with(received, Protocol::ChatCompletionsText, &limits).ok()
        }
    };
    if one(body.clone()).await.is_none() {
        return json!({
            "phase": phase, "shape": shape.name(), "input_bytes": body.len(),
            "error": "the synthetic body is rejected (not a valid measurement input)",
        });
    }
    let before = rss().kb.unwrap_or(0);
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut parsed = Vec::new();
    let mut approved = Vec::new();
    for _ in 0..n {
        match phase {
            "body" => bodies.push(body.clone()),
            "parsed" => {
                if let Some(v) = one(body.clone()).await {
                    parsed.push(v);
                }
            }
            _ => {
                if let Some(v) = one(body.clone()).await
                    && let Ok(s) = inspection
                        .inspect_and_approve(
                            v,
                            ProtocolRoute::new(
                                Protocol::ChatCompletionsText,
                                RouteId::new("synthetic-route"),
                            ),
                        )
                        .await
                {
                    approved.push(s);
                }
            }
        }
    }
    let after = rss().kb.unwrap_or(0);
    let held = bodies.len() + parsed.len() + approved.len();
    let delta_bytes = after.saturating_sub(before).saturating_mul(1024);
    let per = delta_bytes / u64::try_from(held.max(1)).unwrap_or(1);
    let output_bytes = approved.first().map_or(0, |s| s.body().len());
    // The sealed output must not hold the synthetic credential shape (the shapes plant them).
    let output_has_credential_shape = approved
        .iter()
        .any(|s| String::from_utf8_lossy(s.body()).contains("ghp_SYNTHETICREVOKED"));
    json!({
        "kind": "memory_phase",
        "shape": shape.name(),
        "phase": phase,
        "input_bytes": body.len(),
        "retained": held,
        "rss_before_kb": before,
        "rss_after_kb": after,
        "bytes_per_request": per,
        "output_bytes": output_bytes,
        "output_has_credential_shape": output_has_credential_shape,
        "reservation_units": units,
        "reservation_bytes": limits.reservation_bytes(body.len()),
        "bytes_per_request_over_input": per as f64 / body.len().max(1) as f64,
        "bytes_per_request_over_reservation": per as f64 / limits.reservation_bytes(body.len()).max(1) as f64,
    })
}
