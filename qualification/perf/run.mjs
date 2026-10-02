// Synthetic stage-timing and peak-memory measurement through the NON-RELEASE qualification
// build (ADR 0008, ADR 0020). Everything is synthetic: payloads are invented filler and
// revoked-looking tokens, the provider is the scripted fake on loopback, and the output carries
// workload names, sizes, counts, and timings only (never payload bytes).
//
// Run through qualification/run-suites.sh (QUAL_COMMAND) so the fake provider is up:
//   QUAL_COMMAND='node qualification/perf/run.mjs' sh qualification/run-suites.sh --suites none
//
// What it measures, per request, from the gateway's own stage counters (read before and after each
// SEQUENTIAL request, so the delta is exactly that request): admission wait, parse, inspection
// (core work plus worker queueing; serialization is a part of it), serialization, upstream first
// response, upstream total. "core" is inspection minus serialization; "gateway total excluding
// upstream" is client end-to-end minus upstream total (client and loopback overhead included).
// Counters have 1 microsecond resolution, so stages shorter than that read as 0.
//
// HONESTY: results are only as good as the host. The header records CPU count and load average
// before and after; when the one-minute load exceeds half the CPU count the run is labelled
// `provisional`. Nothing here is a performance claim, a limit recommendation, or a default.

import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const need = (n) => {
  const v = process.env[n];
  if (!v) throw new Error(`missing ${n} (run via qualification/run-suites.sh)`);
  return v;
};
const BINARY = need("QUAL_BINARY");
const PROVIDER = need("QUAL_PROVIDER");
const ADMIN = need("QUAL_ADMIN");
const EVIDENCE = need("QUAL_EVIDENCE");
const SYN = JSON.parse(readFileSync(need("QUAL_SYNTHETIC"), "utf8"));
const QUICK = process.argv.includes("--quick");
const scale = QUICK ? 0.1 : 1;
const n = (x) => Math.max(3, Math.round(x * scale));

const providerAddr = new URL(PROVIDER).host;
const tmp = mkdtempSync(path.join(os.tmpdir(), "rsg-perf-"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function pct(sorted, p) {
  if (sorted.length === 0) return null;
  const rank = Math.max(1, Math.ceil((p / 100) * sorted.length));
  return sorted[Math.min(rank, sorted.length) - 1];
}
function dist(values) {
  const v = [...values].sort((a, b) => a - b);
  const sum = v.reduce((a, b) => a + b, 0);
  const r = (x) => (x === null || x === undefined ? null : Math.round(x * 10) / 10);
  return {
    n: v.length,
    min: r(v[0]),
    p50: r(pct(v, 50)),
    p95: r(pct(v, 95)),
    p99: r(pct(v, 99)),
    max: r(v.at(-1)),
    mean: v.length ? Math.round((sum / v.length) * 10) / 10 : null,
  };
}

async function startGateway(inspection) {
  const cfg = {
    schema_version: 1,
    deployment: { listener: { address: "127.0.0.1:0" }, upstream: { provider: "openai" } },
    content: { profile: "full" },
    resources: {
      capacity: { receipt: 16, memory_units: 262144, inspection, upstream: 16, stream: 16 },
    },
  };
  const file = path.join(tmp, `perf-inspection-${inspection}.json`);
  writeFileSync(file, JSON.stringify(cfg));
  const child = spawn(BINARY, ["serve", file, "--fake-provider", providerAddr], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let out = "";
  child.stderr.on("data", () => {});
  const ready = new Promise((resolve, reject) => {
    child.stdout.on("data", (d) => {
      out += d;
      const l = /listening (\S+)/.exec(out);
      const m = /qualification-metrics (\S+)/.exec(out);
      if (l && m) resolve({ base: `http://${l[1]}`, metrics: `http://${m[1]}` });
    });
    child.on("exit", () => reject(new Error("gateway exited early")));
  });
  const { base, metrics } = await ready;
  return { child, base, metrics, inspection };
}

async function stop(g) {
  g.child.kill("SIGTERM");
  await new Promise((r) => g.child.on("exit", r));
}

async function snapshot(g) {
  return (await fetch(g.metrics)).json();
}

/** Resident memory sampler: `ps` polling (any OS) plus VmHWM from the process (Linux). */
function sampleRss(pid) {
  let max = 0;
  const read = () => {
    try {
      const kb = Number(execFileSync("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" }).trim());
      if (Number.isFinite(kb)) max = Math.max(max, kb);
    } catch {
      /* process gone */
    }
  };
  read();
  const timer = setInterval(read, 25);
  return {
    stop: () => {
      clearInterval(timer);
      read();
      return max;
    },
  };
}

function bodyFor(shape, targetBytes, model = "qual-json-ok", stream = false) {
  let text = "";
  let i = 0;
  let secrets = 0;
  while (text.length < targetBytes) {
    if (shape === "many_findings") {
      text += `token ${SYN.secret_prefix}${String(i).padStart(20, "0")} `;
      secrets += 1;
    } else {
      text += `synthetic filler sentence number ${i} `;
    }
    i += 1;
  }
  const doc = { model, messages: [{ role: "user", content: text }] };
  if (stream) doc.stream = true;
  return { body: JSON.stringify(doc), secrets };
}

const STAGES = ["admission_wait", "parse", "inspection", "serialization", "upstream_first_response", "upstream_total"];

async function post(g, body) {
  const t = performance.now();
  const res = await fetch(`${g.base}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${SYN.api_key}` },
    body,
  });
  const text = await res.text();
  return { status: res.status, ms: performance.now() - t, bytes: text.length };
}

/** Sequential requests; per-request stage deltas from the gateway counters. */
async function sequential(g, label, shape, targetBytes, count) {
  await fetch(`${ADMIN}/__admin/reset`, { method: "POST" });
  const { body, secrets } = bodyFor(shape, targetBytes);
  const stage = Object.fromEntries(STAGES.map((s) => [s, []]));
  const e2e = [];
  const gw = [];
  const core = [];
  let failures = 0;
  const statuses = {};
  const rss = sampleRss(g.child.pid);
  // One warm-up request (not recorded): first-use costs are not steady state.
  await post(g, body);
  for (let i = 0; i < count; i++) {
    const before = await snapshot(g);
    const r = await post(g, body);
    const after = await snapshot(g);
    statuses[r.status] = (statuses[r.status] ?? 0) + 1;
    if (r.status !== 200) failures += 1;
    e2e.push(r.ms * 1000);
    for (const s of STAGES) {
      const dc = after.stages[s].count - before.stages[s].count;
      const dt = after.stages[s].total_us - before.stages[s].total_us;
      if (dc === 1) stage[s].push(dt);
    }
    const up = after.stages.upstream_total.total_us - before.stages.upstream_total.total_us;
    gw.push(r.ms * 1000 - up);
    core.push(
      after.stages.inspection.total_us -
        before.stages.inspection.total_us -
        (after.stages.serialization.total_us - before.stages.serialization.total_us),
    );
  }
  const peak = rss.stop();
  const calls = (await (await fetch(`${ADMIN}/__admin/calls`)).json()).calls;
  const forwarded = calls.at(-1)?.body ?? "";
  const final = await snapshot(g);
  return {
    workload: label,
    input_bytes: Buffer.byteLength(body),
    planted_secrets: secrets,
    placeholders_in_forwarded_body: forwarded.split("<SECRET_").length - 1,
    secret_prefix_in_forwarded_body: forwarded.includes("ghp_"),
    requests: count,
    failures,
    status_counts: statuses,
    inspection_capacity: g.inspection,
    stage_us: {
      ...Object.fromEntries(STAGES.map((s) => [s, dist(stage[s])])),
      core_inspection_excl_serialization: dist(core),
      gateway_total_excl_upstream_approx: dist(gw),
      client_end_to_end: dist(e2e),
    },
    peak_rss_kb: { ps_sampled_max: peak, process_vmhwm: final.rss_peak_kb },
  };
}

async function concurrent(g, label, shape, targetBytes, clients, perClient) {
  await fetch(`${ADMIN}/__admin/reset`, { method: "POST" });
  const { body } = bodyFor(shape, targetBytes);
  const e2e = [];
  let failures = 0;
  const statuses = {};
  const before = await snapshot(g);
  const rss = sampleRss(g.child.pid);
  const t0 = performance.now();
  await Promise.all(
    Array.from({ length: clients }, async () => {
      for (let i = 0; i < perClient; i++) {
        const r = await post(g, body);
        statuses[r.status] = (statuses[r.status] ?? 0) + 1;
        if (r.status !== 200) failures += 1;
        e2e.push(r.ms * 1000);
      }
    }),
  );
  const wall = performance.now() - t0;
  const peak = rss.stop();
  const after = await snapshot(g);
  const mean = (s) => {
    const c = after.stages[s].count - before.stages[s].count;
    return c ? Math.round((after.stages[s].total_us - before.stages[s].total_us) / c) : null;
  };
  return {
    workload: label,
    input_bytes: Buffer.byteLength(body),
    clients,
    requests: clients * perClient,
    failures,
    status_counts: statuses,
    inspection_capacity: g.inspection,
    wall_ms: Math.round(wall),
    throughput_rps: Math.round(((clients * perClient) / (wall / 1000)) * 10) / 10,
    client_end_to_end_us: dist(e2e),
    stage_mean_us: Object.fromEntries(STAGES.map((s) => [s, mean(s)])),
    stage_max_us: Object.fromEntries(STAGES.map((s) => [s, after.stages[s].max_us])),
    peak_rss_kb: { ps_sampled_max: peak, process_vmhwm: after.rss_peak_kb },
  };
}

async function sse(g, label, count, readerDelayMs, clients = 1) {
  await fetch(`${ADMIN}/__admin/reset`, { method: "POST" });
  const { body } = bodyFor("no_findings", 2048, "qual-sse-slow", true);
  const first = [];
  const total = [];
  let failures = 0;
  const statuses = {};
  const before = await snapshot(g);
  const rss = sampleRss(g.child.pid);
  await Promise.all(
    Array.from({ length: clients }, async () => {
      for (let i = 0; i < count; i++) {
        const t = performance.now();
        const res = await fetch(`${g.base}/v1/chat/completions`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${SYN.api_key}` },
          body,
        });
        statuses[res.status] = (statuses[res.status] ?? 0) + 1;
        if (res.status !== 200) {
          failures += 1;
          await res.text();
          continue;
        }
        const reader = res.body.getReader();
        let got = false;
        for (;;) {
          const { done } = await reader.read();
          if (done) break;
          if (!got) {
            first.push((performance.now() - t) * 1000);
            got = true;
          }
          if (readerDelayMs) await sleep(readerDelayMs); // a deliberately slow consumer
        }
        total.push((performance.now() - t) * 1000);
      }
    }),
  );
  const peak = rss.stop();
  const after = await snapshot(g);
  const mean = (s) => {
    const c = after.stages[s].count - before.stages[s].count;
    return c ? Math.round((after.stages[s].total_us - before.stages[s].total_us) / c) : null;
  };
  return {
    workload: label,
    streams: clients * count,
    concurrent_streams: clients,
    reader_delay_ms: readerDelayMs,
    provider_pacing_ms: 25,
    failures,
    status_counts: statuses,
    client_first_event_us: dist(first),
    client_stream_total_us: dist(total),
    gateway_stage_mean_us: {
      stream_first_byte: mean("stream_first_byte"),
      stream_total: mean("stream_total"),
      stream_upstream_wait: mean("stream_upstream_wait"),
      stream_downstream_wait: mean("stream_downstream_wait"),
    },
    stream_buffered_peak_bytes: after.stream_buffered_peak,
    streams_ended: after.streams_ended,
    peak_rss_kb: { ps_sampled_max: peak, process_vmhwm: after.rss_peak_kb },
  };
}

const cpus = os.cpus().length;
const loadBefore = os.loadavg();
const results = [];
const concurrency = [];
const streaming = [];

const g2 = await startGateway(2);
results.push(await sequential(g2, "no_findings_4KiB", "no_findings", 4 * 1024, n(200)));
results.push(await sequential(g2, "many_findings_16KiB", "many_findings", 16 * 1024, n(100)));
results.push(await sequential(g2, "large_input_384KiB", "no_findings", 384 * 1024, n(20)));
streaming.push(await sse(g2, "slow_provider_fast_consumer", n(20), 0));
streaming.push(await sse(g2, "slow_provider_slow_consumer", n(10), 20));
streaming.push(await sse(g2, "slow_provider_8_concurrent_streams", n(5), 0, 8));
await stop(g2);

for (const inspection of [1, 2, 4]) {
  const g = await startGateway(inspection);
  concurrency.push(await concurrent(g, "no_findings_4KiB_8_clients", "no_findings", 4 * 1024, 8, n(25)));
  concurrency.push(await concurrent(g, "many_findings_16KiB_8_clients", "many_findings", 16 * 1024, 8, n(10)));
  await stop(g);
}

const loadAfter = os.loadavg();
// The report is a document that is committed under docs/, which the seam scan treats as prose:
// record the build as "qualification binary" without the seam markers.
const version = execFileSync(BINARY, ["--version"], { encoding: "utf8" })
  .trim()
  .replace(/redact-secret-gateway-qualification/, "qualification binary")
  .replace(/\s*\[RSG-[^\]]*\]/, "");
const quiet = Math.max(loadBefore[0], loadAfter[0]) <= cpus * 0.5;
const report = {
  kind: "alpha1-synthetic-stage-timing",
  tool: "qualification/perf/run.mjs (measurement tool; no performance claim)",
  quick_mode: QUICK,
  build: version,
  build_profile_note: "see `profile` below; the qualification build is not the shipped artifact",
  profile: process.env.QUAL_PROFILE ?? "unknown",
  source_commit: process.env.GATEWAY_COMMIT ?? null,
  host: {
    os: `${os.platform()} ${os.release()}`,
    arch: os.arch(),
    cpus,
    load_average_before: loadBefore.map((x) => Math.round(x * 100) / 100),
    load_average_after: loadAfter.map((x) => Math.round(x * 100) / 100),
    node: process.version,
  },
  provisional: !quiet,
  provisional_reason: quiet
    ? null
    : "host not quiet: one-minute load average exceeded half the CPU count during the run; treat every figure as provisional",
  provider: "scripted fake provider on loopback (immediate JSON reply; 25 ms-paced events for streams); not model delay",
  units: "microseconds unless a key says otherwise; peak memory in KiB",
  config: "profile full; capacity receipt 16, memory_units 262144, upstream 16, stream 16; inspection as listed; all limits at their provisional defaults",
  sequential_stage_timings: results,
  concurrent_runs: concurrency,
  slow_sse: streaming,
};
mkdirSync(EVIDENCE, { recursive: true });
const outFile = path.join(EVIDENCE, QUICK ? "perf-results-quick.json" : "perf-results.json");
writeFileSync(outFile, `${JSON.stringify(report, null, 2)}\n`);
console.log(`wrote ${path.basename(outFile)} (provisional: ${!quiet}; load before ${loadBefore[0].toFixed(2)} on ${cpus} cpus)`);
for (const r of results) {
  console.log(
    `${r.workload}: gateway p50 ${r.stage_us.gateway_total_excl_upstream_approx.p50}us p95 ${r.stage_us.gateway_total_excl_upstream_approx.p95}us; core p50 ${r.stage_us.core_inspection_excl_serialization.p50}us; rss max ${r.peak_rss_kb.ps_sampled_max}KiB; failures ${r.failures}`,
  );
}
