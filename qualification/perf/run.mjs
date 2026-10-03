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

import { spawn, execFile, execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
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

// ---------------------------------------------------------------------------------------------
// Aggregate load qualification (#58, Alpha 2). `--aggregate [--quick] [--runs N]`.
//
// Same stack, same tool, same synthetic data: this adds Alpha 2 body shapes (tool history, tool
// definitions, metadata, node-dense schemas), combined load (large and many-finding JSON, queued
// inspections, concurrent provider calls, slow SSE readers, finite sockets), occupancy samples
// (admission counters read from the gateway), per-stage percentiles under load (the gateway's own
// log-bucketed stage histograms; a percentile is the bucket's upper edge, at most 25% above the true
// value), true peak resident memory (`/usr/bin/time -l` on macOS, `VmHWM` on Linux), overload, and
// recovery. Output carries counts, sizes, and timings only.
//
// Quiet-host protocol: before each pass the load average is read; when the one-minute value is above
// 25% of the CPU count the pass waits (up to AGG_QUIET_WAIT_MS, default 180000) for it to fall. A pass
// that starts or ends above the threshold is labelled provisional. Nothing is ever labelled quiet that
// was not.
// ---------------------------------------------------------------------------------------------

const AGGREGATE = process.argv.includes("--aggregate");
const RUNS = (() => {
  const i = process.argv.indexOf("--runs");
  return i > 0 ? Math.max(1, Number(process.argv[i + 1])) : QUICK ? 1 : 5;
})();
const QUIET_FRACTION = 0.25;
const QUIET_WAIT_MS = Number(process.env.AGG_QUIET_WAIT_MS ?? (QUICK ? 0 : 180000));
const DARWIN = process.platform === "darwin";

const execp = (cmd, args) =>
  new Promise((resolve) => {
    execFile(cmd, args, { encoding: "utf8", maxBuffer: 8 * 1024 * 1024 }, (err, stdout) => resolve(err ? "" : stdout));
  });

const tok = (n) => `${SYN.secret_prefix}${String(n).padStart(20, "0")}`;
function fill(n, len) {
  let t = "";
  let i = 0;
  while (t.length < len) {
    t += `synthetic filler ${n} ${i} `;
    i += 1;
  }
  return t.slice(0, len);
}

/** Request bodies. Mirrors tests/support/workloads.rs for the Alpha 2 shapes. */
function shapeBody(shape, model = "qual-json-ok") {
  const base = (extra, messages) => JSON.stringify({ model, messages, ...extra });
  const user = [{ role: "user", content: "q" }];
  switch (shape) {
    case "small_4KiB":
      return { body: base({}, [{ role: "user", content: fill(0, 4096) }]), expect_findings: 0 };
    case "many_findings_16KiB": {
      let t = "";
      let n = 0;
      while (t.length < 16384) t += `token ${tok(n++)} `;
      return { body: base({}, [{ role: "user", content: t }]), expect_findings: n };
    }
    case "large_500KiB":
      return { body: base({}, [{ role: "user", content: fill(1, 500 * 1024) }]), expect_findings: 0 };
    case "large_many_findings_500KiB": {
      let t = fill(2, 460 * 1024);
      let n = 0;
      for (; n < 900; n++) t += ` ${tok(n)}`;
      return { body: base({}, [{ role: "user", content: t }]), expect_findings: n };
    }
    case "findings_over_limit": {
      let t = "";
      for (let n = 0; n < 1300; n++) t += `${tok(n)} `;
      return { body: base({}, [{ role: "user", content: t }]), expect_findings: 1300 };
    }
    case "tool_history": {
      const per = Math.max(32, Math.floor(600000 / (48 * 8)));
      const messages = [{ role: "user", content: "run the lookups" }];
      let n = 0;
      for (let t = 0; t < 48; t++) {
        const calls = [];
        for (let c = 0; c < 4; c++) {
          const args = { query: `${fill(n, per)} ${tok(n)}`, filters: { a: { b: { c: { d: `v${n}` } } } }, limit: 5 };
          calls.push({ id: `call_${t}_${c}`, type: "function", function: { name: "lookup_record", arguments: JSON.stringify(args) } });
          n++;
        }
        messages.push({ role: "assistant", content: null, tool_calls: calls });
        for (let c = 0; c < 4; c++) {
          messages.push({ role: "tool", tool_call_id: `call_${t}_${c}`, content: `${fill(n, per)} ${tok(n)}` });
          n++;
        }
      }
      return { body: base({}, messages), expect_findings: n };
    }
    case "tool_defs": {
      const desc = Math.min(1024, Math.max(16, Math.floor(700000 / (64 * 32))));
      const tools = [];
      let secrets = 0;
      for (let k = 0; k < 64; k++) {
        const properties = {};
        const required = [];
        for (let j = 0; j < 32; j++) {
          let text = fill(k * 32 + j, desc);
          if (j % 8 === 0) {
            text += ` ${tok(k * 32 + j)}`;
            secrets++;
          }
          properties[`p_${j}`] = { type: "string", description: text };
          if (j < 4) required.push(`p_${j}`);
        }
        tools.push({ type: "function", function: { name: `tool_${k}`, description: fill(k, desc), parameters: { type: "object", properties, required } } });
      }
      return { body: base({ tools }, user), expect_findings: secrets };
    }
    case "metadata": {
      const metadata = {};
      let secrets = 0;
      for (let i = 0; i < 16; i++) {
        let v = fill(i, 440);
        if (i % 2 === 0) {
          v += ` ${tok(i)}`;
          secrets++;
        }
        metadata[`meta_key_${String(i).padStart(2, "0")}`] = v.slice(0, 512);
      }
      return { body: base({ metadata }, [{ role: "user", content: "hello" }]), expect_findings: secrets };
    }
    case "node_dense": {
      const tools = [];
      for (let k = 0; k < 5; k++) {
        const properties = {};
        for (let j = 0; j < 64; j++) properties[`p_${j}`] = { type: "string", enum: Array.from({ length: 40 }, (_, e) => `v${e}`) };
        tools.push({ type: "function", function: { name: `dense_${k}`, parameters: { type: "object", properties } } });
      }
      return { body: base({ tools }, user), expect_findings: 0 };
    }
    default:
      throw new Error("unknown shape");
  }
}

const SHAPES = ["small_4KiB", "many_findings_16KiB", "large_500KiB", "large_many_findings_500KiB", "tool_history", "tool_defs", "metadata", "node_dense"];

function histDelta(a, b) {
  const m = new Map((a ?? []).map(([u, n]) => [u, n]));
  return (b ?? []).map(([u, n]) => [u, n - (m.get(u) ?? 0)]).filter(([, n]) => n > 0);
}
function histPct(h, p) {
  const total = h.reduce((s, [, n]) => s + n, 0);
  if (!total) return null;
  const rank = Math.max(1, Math.ceil((p / 100) * total));
  let cum = 0;
  for (const [u, n] of h) {
    cum += n;
    if (cum >= rank) return u;
  }
  return h.at(-1)[0];
}
const AGG_STAGES = [...STAGES, "stream_first_byte", "stream_total", "stream_upstream_wait", "stream_downstream_wait"];
function stageWindow(before, after) {
  const out = {};
  for (const s of AGG_STAGES) {
    const h = histDelta(before.stages[s].hist, after.stages[s].hist);
    const count = after.stages[s].count - before.stages[s].count;
    if (!count) continue;
    out[s] = {
      n: count,
      mean_us: Math.round((after.stages[s].total_us - before.stages[s].total_us) / count),
      p50_us: histPct(h, 50),
      p95_us: histPct(h, 95),
      p99_us: histPct(h, 99),
      max_us_since_start: after.stages[s].max_us,
    };
  }
  return out;
}

const ZERO_ADM = ["receipt_in_use", "memory_units_in_use", "inspection_in_use", "upstream_in_use", "stream_in_use", "waiting"];

/** Gateway under `/usr/bin/time -l` on macOS so the true peak resident size is known at exit. */
async function startAggGateway(cap, limits = {}, content = {}) {
  const cfg = {
    schema_version: 1,
    deployment: { listener: { address: "127.0.0.1:0" }, upstream: { provider: "openai" } },
    content: { profile: "full", ...content },
    resources: { capacity: cap, limits },
  };
  const file = path.join(tmp, `agg-${process.pid}-${Math.random().toString(36).slice(2)}.json`);
  writeFileSync(file, JSON.stringify(cfg));
  const args = ["serve", file, "--fake-provider", providerAddr];
  const child = DARWIN
    ? spawn("/usr/bin/time", ["-l", BINARY, ...args], { stdio: ["ignore", "pipe", "pipe"] })
    : spawn(BINARY, args, { stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  let err = "";
  child.stderr.on("data", (d) => {
    err += d;
  });
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
  let pid = child.pid;
  if (DARWIN) {
    const kids = (await execp("pgrep", ["-P", String(child.pid)])).trim().split("\n").filter(Boolean);
    pid = Number(kids[0]);
  }
  const exited = new Promise((r) => child.on("exit", r));
  return { child, pid, base, metrics, cap, exited, err: () => err, hostPort: new URL(base).port };
}

async function stopAggGateway(g) {
  try {
    process.kill(g.pid, "SIGTERM");
  } catch {
    /* gone */
  }
  await g.exited;
  if (DARWIN) {
    const m = /(\d+)\s+maximum resident set size/.exec(g.err());
    const f = /(\d+)\s+peak memory footprint/.exec(g.err());
    return { max_rss_kb: m ? Math.round(Number(m[1]) / 1024) : null, peak_footprint_kb: f ? Math.round(Number(f[1]) / 1024) : null, source: "time -l (ru_maxrss, true peak)" };
  }
  return { max_rss_kb: g.lastVmhwm ?? null, peak_footprint_kb: null, source: "VmHWM (true peak)" };
}

async function procCounts(g) {
  let rss = null;
  let threads = null;
  let fds = null;
  let sockets = null;
  if (DARWIN) {
    rss = Number((await execp("ps", ["-o", "rss=", "-p", String(g.pid)])).trim()) || null;
    const t = (await execp("ps", ["-M", "-p", String(g.pid)])).split("\n").filter(Boolean);
    threads = Math.max(0, t.length - 1);
    const l = (await execp("lsof", ["-n", "-P", "-p", String(g.pid)])).split("\n").filter(Boolean);
    fds = Math.max(0, l.length - 1);
    sockets = l.filter((x) => / TCP /.test(x)).length;
  } else {
    try {
      const status = readFileSync(`/proc/${g.pid}/status`, "utf8");
      rss = Number(/VmRSS:\s+(\d+)/.exec(status)?.[1]) || null;
      threads = Number(/Threads:\s+(\d+)/.exec(status)?.[1]) || null;
      const dir = readdirSync(`/proc/${g.pid}/fd`);
      fds = dir.length;
      sockets = dir.filter((f) => {
        try {
          return readlinkSync(`/proc/${g.pid}/fd/${f}`).startsWith("socket:");
        } catch {
          return false;
        }
      }).length;
    } catch {
      /* gone */
    }
  }
  return { rss_kb: rss, threads, fds, sockets };
}

function startSampler(g) {
  const peak = Object.fromEntries([...ZERO_ADM, "rss_kb", "threads", "fds", "sockets", "snapshots"].map((k) => [k, 0]));
  let stopped = false;
  const fast = (async () => {
    while (!stopped) {
      try {
        const s = await snapshot(g);
        peak.snapshots += 1;
        if (s.rss_peak_kb) g.lastVmhwm = s.rss_peak_kb;
        for (const k of ZERO_ADM) peak[k] = Math.max(peak[k], s.admission?.[k] ?? 0);
        peak.memory_units_total = s.admission?.memory_units_total ?? null;
      } catch {
        /* gateway busy or gone */
      }
      await sleep(15);
    }
  })();
  const slow = (async () => {
    while (!stopped) {
      const c = await procCounts(g);
      for (const k of ["rss_kb", "threads", "fds", "sockets"]) peak[k] = Math.max(peak[k], c[k] ?? 0);
      await sleep(300);
    }
  })();
  return {
    stop: async () => {
      stopped = true;
      await Promise.all([fast, slow]);
      return peak;
    },
  };
}

async function providerCall(pathAndQuery, method = "GET") {
  const r = await fetch(`${ADMIN}${pathAndQuery}`, { method });
  return r.json();
}
async function providerFresh() {
  await providerCall("/__admin/reset", "POST");
  await providerCall("/__admin/mode?keep_bodies=0", "POST");
}

async function aggCall(g, body, { abortMs = null } = {}) {
  const t = performance.now();
  const ac = new AbortController();
  const timer = abortMs === null ? null : setTimeout(() => ac.abort(), abortMs);
  try {
    const res = await fetch(`${g.base}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${SYN.api_key}` },
      body,
      signal: ac.signal,
    });
    const text = await res.text();
    let code = "ok";
    if (res.status !== 200) {
      try {
        code = JSON.parse(text)?.error?.code ?? "unparsed";
      } catch {
        code = "unparsed";
      }
    }
    return { status: res.status, code, ms: performance.now() - t };
  } catch {
    return { status: 0, code: ac.signal.aborted ? "client_abort" : "connection_closed", ms: performance.now() - t };
  } finally {
    if (timer) clearTimeout(timer);
  }
}

class Tally {
  constructor() {
    this.n = 0;
    this.byStatus = {};
    this.byCode = {};
    this.ok_ms = [];
  }
  add(r) {
    this.n += 1;
    this.byStatus[r.status] = (this.byStatus[r.status] ?? 0) + 1;
    const key = r.status === 200 ? "ok" : r.code;
    this.byCode[key] = (this.byCode[key] ?? 0) + 1;
    if (r.status === 200) this.ok_ms.push(r.ms * 1000);
  }
  json() {
    return { requests: this.n, status_counts: this.byStatus, outcome_counts: this.byCode, ok_latency_us: dist(this.ok_ms) };
  }
}

/** Poll until every admission counter is zero; resolves to elapsed ms or null. */
async function awaitDrained(g, limitMs = 15000) {
  const t = performance.now();
  while (performance.now() - t < limitMs) {
    const s = await snapshot(g);
    if (ZERO_ADM.every((k) => (s.admission?.[k] ?? 1) === 0)) return Math.round(performance.now() - t);
    await sleep(5);
  }
  return null;
}

async function recoveryProbe(g, n = 10) {
  const { body } = shapeBody("small_4KiB");
  let okCount = 0;
  for (let i = 0; i < n; i++) if ((await aggCall(g, body)).status === 200) okCount += 1;
  return okCount;
}

/** One gateway per shape: sequential per-request stage percentiles, forwarded-body checks, true peak. */
async function aggStages(cap) {
  const out = {};
  for (const shape of SHAPES) {
    const g = await startAggGateway(cap, {});
    await providerFresh();
    const { body, expect_findings } = shapeBody(shape);
    const baseline = await procCounts(g);
    const stage = Object.fromEntries(STAGES.map((s) => [s, []]));
    const e2e = [];
    const gw = [];
    const statuses = {};
    await aggCall(g, body); // warm-up, not recorded
    const count = n(shape.startsWith("small") || shape.startsWith("metadata") || shape.startsWith("node") ? 60 : 25);
    for (let i = 0; i < count; i++) {
      const before = await snapshot(g);
      const r = await aggCall(g, body);
      const after = await snapshot(g);
      statuses[r.status] = (statuses[r.status] ?? 0) + 1;
      e2e.push(r.ms * 1000);
      for (const s of STAGES) {
        if (after.stages[s].count - before.stages[s].count === 1) stage[s].push(after.stages[s].total_us - before.stages[s].total_us);
      }
      gw.push(r.ms * 1000 - (after.stages.upstream_total.total_us - before.stages.upstream_total.total_us));
    }
    const held = await procCounts(g);
    const stats = await providerCall("/__admin/stats");
    const drained = await awaitDrained(g);
    const peak = await stopAggGateway(g);
    out[shape] = {
      input_bytes: Buffer.byteLength(body),
      expected_findings: expect_findings,
      requests: count,
      status_counts: statuses,
      provider_received: stats.received,
      provider_bodies_with_secret_prefix: stats.secret_bodies,
      stage_us: { ...Object.fromEntries(STAGES.map((s) => [s, dist(stage[s])])), gateway_total_excl_upstream_approx: dist(gw), client_end_to_end: dist(e2e) },
      rss_kb: { baseline: baseline.rss_kb, after_requests: held.rss_kb, true_peak: peak.max_rss_kb, peak_footprint: peak.peak_footprint_kb },
      drained_ms: drained,
    };
  }
  return out;
}

/** Every pre-forward rejection sends zero upstream body (provider counters, not logs). */
async function aggRejections() {
  const g = await startAggGateway({ receipt: 8, memory_units: 65536, inspection: 2, upstream: 8, stream: 8 }, {});
  await providerFresh();
  const big = JSON.stringify({ model: "qual-json-ok", messages: [{ role: "user", content: "x".repeat(1100 * 1024) }] });
  const cases = [
    ["findings_over_limit", shapeBody("findings_over_limit").body],
    ["body_over_limit", big],
    ["unsupported_field", JSON.stringify({ model: "qual-json-ok", messages: [{ role: "user", content: "q" }], logit_bias: { 1: 1 } })],
    ["malformed_json", '{"model":"qual-json-ok","messages":['],
    ["duplicate_key", '{"model":"qual-json-ok","model":"x","messages":[{"role":"user","content":"q"}]}'],
  ];
  const result = {};
  for (const [name, body] of cases) {
    const t = new Tally();
    for (let i = 0; i < n(10); i++) t.add(await aggCall(g, body));
    result[name] = t.json().outcome_counts;
  }
  const stats = await providerCall("/__admin/stats");
  const drained = await awaitDrained(g);
  await stopAggGateway(g);
  return { cases: result, provider_received_total: stats.received, drained_ms: drained };
}

/** Held-request memory: K requests parked at the provider keep their reservation and sealed body. */
async function aggHeld() {
  const out = {};
  for (const shape of ["metadata", "node_dense", "tool_defs", "tool_history", "large_500KiB", "many_findings_16KiB"]) {
    for (const k of QUICK ? [4] : [1, 8]) {
      const g = await startAggGateway({ receipt: 64, memory_units: 262144, inspection: 16, upstream: 64, stream: 16 }, {});
      await providerFresh();
      const warm = shapeBody(shape, "qual-json-ok");
      await aggCall(g, warm.body);
      const base = await procCounts(g);
      const { body } = shapeBody(shape, "qual-hang");
      const aborts = Array.from({ length: k }, () => new AbortController());
      const calls = aborts.map(async (ac) => {
        try {
          const res = await fetch(`${g.base}/v1/chat/completions`, { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${SYN.api_key}` }, body, signal: ac.signal });
          await res.text();
        } catch {
          /* aborted */
        }
      });
      let held = null;
      const t0 = performance.now();
      while (performance.now() - t0 < 10000) {
        const s = await snapshot(g);
        if (s.admission.upstream_in_use === k) {
          held = s.admission;
          break;
        }
        await sleep(5);
      }
      await sleep(250);
      const rssHeld = Math.max(...[(await procCounts(g)).rss_kb ?? 0, (await procCounts(g)).rss_kb ?? 0]);
      for (const ac of aborts) ac.abort();
      await Promise.all(calls);
      const drained = await awaitDrained(g);
      const peak = await stopAggGateway(g);
      const unitBytes = held ? held.memory_units_in_use * 1024 : null;
      out[`${shape}_x${k}`] = {
        input_bytes: Buffer.byteLength(body),
        held_requests: k,
        reservation_units_in_use: held?.memory_units_in_use ?? null,
        reservation_bytes: unitBytes,
        rss_growth_held_bytes: rssHeld && base.rss_kb ? (rssHeld - base.rss_kb) * 1024 : null,
        true_peak_growth_bytes: peak.max_rss_kb && base.rss_kb ? (peak.max_rss_kb - base.rss_kb) * 1024 : null,
        held_growth_over_reservation: unitBytes && rssHeld && base.rss_kb ? Math.round(((rssHeld - base.rss_kb) * 1024 * 1000) / unitBytes) / 1000 : null,
        peak_growth_over_reservation: unitBytes && peak.max_rss_kb && base.rss_kb ? Math.round(((peak.max_rss_kb - base.rss_kb) * 1024 * 1000) / unitBytes) / 1000 : null,
        drained_ms: drained,
      };
    }
  }
  return out;
}

const MIX = [
  ["small_4KiB", 40],
  ["many_findings_16KiB", 10],
  ["large_500KiB", 8],
  ["large_many_findings_500KiB", 4],
  ["tool_history", 10],
  ["tool_defs", 10],
  ["metadata", 8],
  ["node_dense", 10],
];
function pick(weights, r) {
  let acc = 0;
  const total = weights.reduce((s, [, w]) => s + w, 0);
  for (const [name, w] of weights) {
    acc += w;
    if (r * total < acc) return name;
  }
  return weights.at(-1)[0];
}

/** Combined load: mixed shapes, slow provider replies, slow SSE readers, finite sockets; then recovery. */
async function aggMixed(label, cap, limits, load) {
  const g = await startAggGateway(cap, limits);
  await providerFresh();
  const bodies = Object.fromEntries(SHAPES.map((s) => [s, { json: shapeBody(s, "qual-json-slow").body, fast: shapeBody(s, "qual-json-ok").body }]));
  const sseBody = JSON.stringify({ model: "qual-sse-hang", stream: true, messages: [{ role: "user", content: "q" }] });
  const baseline = await procCounts(g);
  const before = await snapshot(g);
  const sampler = startSampler(g);
  const t0 = performance.now();
  const endAt = t0 + load.seconds * 1000;
  const byClass = {};
  const tally = (c) => (byClass[c] ??= new Tally());
  const sse = new Tally();
  let sseHeld = 0;

  const flood = [];
  let floodClosedEarly = 0;
  for (let i = 0; i < load.flood; i++) {
    const s = net.connect(Number(g.hostPort), "127.0.0.1");
    s.on("error", () => {});
    s.on("close", () => {
      if (performance.now() < endAt) floodClosedEarly += 1;
    });
    flood.push(s);
  }

  const workers = [];
  for (let c = 0; c < load.clients; c++) {
    workers.push(
      (async () => {
        let i = c;
        while (performance.now() < endAt) {
          const shape = pick(MIX, ((i++ * 0.61803398875 + c * 0.37) % 1 + 1) % 1);
          const slow = (i + c) % 3 === 0; // a third of the calls hold an upstream permit for the provider delay
          tally(shape).add(await aggCall(g, slow ? bodies[shape].json : bodies[shape].fast));
        }
      })(),
    );
  }
  for (let c = 0; c < load.sse_readers; c++) {
    workers.push(
      (async () => {
        while (performance.now() < endAt) {
          const ac = new AbortController();
          const t = performance.now();
          try {
            const res = await fetch(`${g.base}/v1/chat/completions`, { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${SYN.api_key}` }, body: sseBody, signal: ac.signal });
            if (res.status !== 200) {
              sse.add({ status: res.status, code: "stream_refused", ms: 0 });
              await res.text();
              await sleep(100);
              continue;
            }
            const reader = res.body.getReader();
            await reader.read();
            sseHeld += 1;
            await sleep(load.sse_hold_ms); // a reader that stops reading while holding its stream permit
            ac.abort();
            sse.add({ status: 200, code: "ok", ms: performance.now() - t });
          } catch {
            sse.add({ status: 0, code: "connection_closed", ms: 0 });
            await sleep(100);
          }
        }
      })(),
    );
  }
  await Promise.all(workers);
  const loadEnd = performance.now();
  const peak = await sampler.stop();
  const during = await snapshot(g);
  const stages = stageWindow(before, during);

  // Recovery: stop the load completely (clients stopped; flood sockets closed) and watch the counters.
  for (const s of flood) s.destroy();
  const drained = await awaitDrained(g, 20000);
  let settle = null;
  const ts = performance.now();
  while (performance.now() - ts < 5000) {
    const c = await procCounts(g);
    if (c.sockets !== null && c.sockets <= baseline.sockets + 1) {
      settle = Math.round(performance.now() - ts);
      break;
    }
    await sleep(100);
  }
  const after = await procCounts(g);
  const probes = await recoveryProbe(g);
  const stats = await providerCall("/__admin/stats");
  const final = await snapshot(g);
  const tallies = Object.fromEntries(Object.entries(byClass).map(([k, v]) => [k, v.json()]));
  let ok200 = 0;
  let ambiguous = 0;
  let rejected = 0;
  for (const v of Object.values(byClass)) {
    ok200 += v.byStatus[200] ?? 0;
    ambiguous += (v.byStatus[0] ?? 0) + Object.entries(v.byStatus).filter(([s]) => ["502", "504", "500"].includes(s)).reduce((a, [, c]) => a + c, 0);
    rejected += Object.entries(v.byStatus).filter(([s]) => s !== "0" && s !== "200" && !["502", "504", "500"].includes(s)).reduce((a, [, c]) => a + c, 0);
  }
  // Streams started but never answered by the provider scenario are also provider calls.
  const sseCalls = sseHeld + (sse.byStatus[0] ?? 0);
  const trueRss = await stopAggGateway(g);
  return {
    label,
    capacity: cap,
    limits,
    load,
    wall_s: Math.round(((loadEnd - t0) / 1000) * 10) / 10,
    by_shape: tallies,
    sse_readers: sse.json(),
    flood: { sockets_opened: load.flood, closed_by_gateway_while_loaded: floodClosedEarly },
    peaks_sampled: peak,
    stage_under_load: stages,
    invariants: {
      provider_received: stats.received,
      json_ok_200: ok200,
      json_ambiguous_outcomes: ambiguous,
      json_rejected_before_forward: rejected,
      sse_streams_opened: sseCalls,
      provider_calls_not_explained_by_forwarded_requests: Math.max(0, stats.received - ok200 - ambiguous - sseCalls - probes),
      provider_bodies_with_secret_prefix: stats.secret_bodies,
      inspection_in_use_peak_within_capacity: peak.inspection_in_use <= cap.inspection,
      memory_units_in_use_peak_within_budget: peak.memory_units_in_use <= cap.memory_units,
      upstream_in_use_peak_within_capacity: peak.upstream_in_use <= cap.upstream,
      stream_in_use_peak_within_capacity: peak.stream_in_use <= cap.stream,
      receipt_in_use_peak_within_capacity: peak.receipt_in_use <= cap.receipt,
    },
    recovery: {
      counters_zero_after_ms: drained,
      sockets_back_to_baseline_after_ms: settle,
      sockets_baseline: baseline.sockets,
      sockets_after: after.sockets,
      threads_baseline: baseline.threads,
      threads_after: after.threads,
      rss_kb_baseline: baseline.rss_kb,
      rss_kb_after_recovery: after.rss_kb,
      probe_requests_ok_of_10: probes,
      admission_after: final.admission,
    },
    true_peak_rss: trueRss,
    baseline_rss_kb: baseline.rss_kb,
  };
}

/** Large versus small requests under a tight memory budget: nobody starves, and what the wait queue does. */
async function aggStarvation() {
  const cap = { receipt: 16, memory_units: 6000, inspection: 4, upstream: 16, stream: 8 };
  const limits = { admission_wait_ms: 250, admission_queue: 16 };
  const seconds = QUICK ? 2 : 6;
  const out = {};
  for (const variant of ["mixed_large_and_small", "small_only_control"]) {
    const g = await startAggGateway(cap, limits);
    await providerFresh();
    const large = shapeBody("large_500KiB").body;
    const small = shapeBody("small_4KiB").body;
    const before = await snapshot(g);
    const sampler = startSampler(g);
    const endAt = performance.now() + seconds * 1000;
    const tl = new Tally();
    const ts = new Tally();
    const clients = [];
    const largeClients = variant === "mixed_large_and_small" ? 6 : 0;
    for (let c = 0; c < largeClients; c++) clients.push((async () => { while (performance.now() < endAt) tl.add(await aggCall(g, large)); })());
    for (let c = 0; c < 10; c++) clients.push((async () => { while (performance.now() < endAt) ts.add(await aggCall(g, small)); })());
    await Promise.all(clients);
    const peak = await sampler.stop();
    const after = await snapshot(g);
    const drained = await awaitDrained(g);
    await stopAggGateway(g);
    out[variant] = {
      capacity: cap,
      limits,
      large_reservation_units_approx: Math.ceil((4 * 500 * 1024 + 16384 * 128) / 1024),
      large: tl.json(),
      small: ts.json(),
      stage_under_load: stageWindow(before, after),
      peaks_sampled: peak,
      drained_ms: drained,
    };
  }
  return out;
}

/** Repeated cancellation while synchronous inspection runs: occupancy never exceeds capacity; counts return to zero. */
async function aggCancellation() {
  const cap = { receipt: 16, memory_units: 65536, inspection: 4, upstream: 16, stream: 8 };
  const g = await startAggGateway(cap, {});
  await providerFresh();
  const heavy = shapeBody("large_many_findings_500KiB").body;
  const before = await snapshot(g);
  const sampler = startSampler(g);
  const seconds = QUICK ? 2 : 6;
  const endAt = performance.now() + seconds * 1000;
  const t = new Tally();
  const workers = [];
  for (let c = 0; c < 16; c++) {
    workers.push(
      (async () => {
        let i = c;
        while (performance.now() < endAt) t.add(await aggCall(g, heavy, { abortMs: 2 + ((i++ * 7) % 9) }));
      })(),
    );
  }
  await Promise.all(workers);
  const peak = await sampler.stop();
  const after = await snapshot(g);
  const drained = await awaitDrained(g);
  const providerBefore = await providerCall("/__admin/stats");
  const probes = await recoveryProbe(g, 5);
  const trueRss = await stopAggGateway(g);
  return {
    capacity: cap,
    seconds,
    requests: t.json(),
    inspections_awaited_to_completion: after.stages.inspection.count - before.stages.inspection.count,
    worker_jobs_completed_approx: after.stages.serialization.count - before.stages.serialization.count,
    provider_received_before_probes: providerBefore.received,
    provider_bodies_with_secret_prefix: providerBefore.secret_bodies,
    peaks_sampled: peak,
    inspection_peak_within_capacity: peak.inspection_in_use <= cap.inspection,
    counters_zero_after_ms: drained,
    probe_requests_ok_of_5: probes,
    stage_under_load: stageWindow(before, after),
    true_peak_rss: trueRss,
  };
}

/** Finite sockets alone: how many connections a gateway keeps, and that it still answers. */
async function aggSockets() {
  const max = 64;
  const g = await startAggGateway({ receipt: 8, memory_units: 65536, inspection: 2, upstream: 8, stream: 8 }, { max_connections: max });
  const baseline = await procCounts(g);
  const socks = [];
  let closed = 0;
  for (let i = 0; i < 200; i++) {
    const s = net.connect(Number(g.hostPort), "127.0.0.1");
    s.on("error", () => {});
    s.on("close", () => {
      closed += 1;
    });
    socks.push(s);
  }
  await sleep(1000);
  const held = await procCounts(g);
  const open = socks.filter((s) => !s.destroyed).length;
  for (const s of socks) s.destroy();
  const ts = performance.now();
  let settle = null;
  while (performance.now() - ts < 5000) {
    const c = await procCounts(g);
    if (c.sockets <= baseline.sockets + 1) {
      settle = Math.round(performance.now() - ts);
      break;
    }
    await sleep(100);
  }
  const probes = await recoveryProbe(g, 5);
  await stopAggGateway(g);
  return { max_connections: max, attempted: 200, open_after_1s: open, closed_by_gateway: closed, gateway_sockets_baseline: baseline.sockets, gateway_sockets_held: held.sockets, gateway_threads_held: held.threads, gateway_rss_kb_baseline: baseline.rss_kb, gateway_rss_kb_held: held.rss_kb, sockets_back_after_ms: settle, probe_requests_ok_of_5: probes };
}

function flat(obj, prefix = "", acc = {}) {
  for (const [k, v] of Object.entries(obj ?? {})) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (typeof v === "number" && Number.isFinite(v)) acc[key] = v;
    else if (v && typeof v === "object" && !Array.isArray(v)) flat(v, key, acc);
  }
  return acc;
}
function spread(runs) {
  const keys = new Set(runs.flatMap((r) => Object.keys(r)));
  const out = {};
  for (const k of keys) {
    const v = runs.map((r) => r[k]).filter((x) => typeof x === "number").sort((a, b) => a - b);
    if (!v.length) continue;
    const mid = v.length % 2 ? v[(v.length - 1) / 2] : (v[v.length / 2 - 1] + v[v.length / 2]) / 2;
    out[k] = { median: Math.round(mid * 10) / 10, min: v[0], max: v.at(-1), runs: v.length };
  }
  return out;
}

async function hostSnapshot() {
  const cpus = os.cpus().length;
  const uptime = (await execp("uptime", [])).trim();
  const top = DARWIN ? await execp("ps", ["-Ao", "pcpu,pid,comm", "-r"]) : await execp("ps", ["-eo", "pcpu,pid,comm", "--sort=-pcpu"]);
  const topLines = top.split("\n").slice(1, 6).map((l) => l.trim().replace(/\s+/, " ").replace(/^(\S+ \S+ ).*\//, "$1")).filter(Boolean);
  return { at: new Date().toISOString(), uptime, load1: os.loadavg()[0], threshold_load1: Math.round(cpus * QUIET_FRACTION * 100) / 100, top_cpu: topLines };
}

async function waitForQuiet() {
  const first = await hostSnapshot();
  const start = performance.now();
  let cur = first;
  while (cur.load1 > cur.threshold_load1 && performance.now() - start < QUIET_WAIT_MS) {
    await sleep(15000);
    cur = await hostSnapshot();
  }
  return { before_wait: first, at_start: cur, waited_s: Math.round((performance.now() - start) / 1000), quiet_at_start: cur.load1 <= cur.threshold_load1 };
}

async function rustcVersion() {
  return (await execp("rustc", ["--version"])).trim() || null;
}

async function aggregateMain() {
  const cpus = os.cpus().length;
  const capStd = { receipt: 16, memory_units: 262144, inspection: 4, upstream: 16, stream: 16 };
  const capTight = { receipt: 8, memory_units: 16384, inspection: 4, upstream: 8, stream: 4 };
  const limTight = { max_connections: 64, admission_wait_ms: 250, admission_queue: 16 };
  const load = { clients: 24, sse_readers: 6, sse_hold_ms: 1500, flood: 40, seconds: QUICK ? 2 : 8 };
  const runs = [];
  for (let r = 0; r < RUNS; r++) {
    const quiet = await waitForQuiet();
    const t0 = Date.now();
    const run = { run: r + 1, host_before: quiet };
    run.stages_by_shape = await aggStages(capStd);
    run.pre_forward_rejections = await aggRejections();
    run.held_request_memory = await aggHeld();
    run.mixed_overload = await aggMixed("tight_capacity_finite_sockets", capTight, limTight, load);
    run.mixed_ample = await aggMixed("standard_capacity_default_sockets", capStd, {}, { ...load, flood: 40 });
    run.starvation = await aggStarvation();
    run.cancellation_storm = await aggCancellation();
    run.sockets_only = await aggSockets();
    const end = await hostSnapshot();
    run.host_after = end;
    run.duration_s = Math.round((Date.now() - t0) / 1000);
    run.provisional = !(quiet.quiet_at_start && end.load1 <= end.threshold_load1);
    runs.push(run);
    console.log(`run ${r + 1}/${RUNS}: load1 start ${quiet.at_start.load1.toFixed(2)} end ${end.load1.toFixed(2)} (threshold ${end.threshold_load1}) provisional=${run.provisional} ${run.duration_s}s`);
  }

  // Scalars for median/spread: the headline per-run numbers.
  const headline = runs.map((run) => {
    const h = {};
    for (const [shape, v] of Object.entries(run.stages_by_shape)) {
      for (const s of ["admission_wait", "parse", "inspection", "serialization", "upstream_first_response", "upstream_total"]) {
        h[`seq.${shape}.${s}.p50`] = v.stage_us[s].p50;
        h[`seq.${shape}.${s}.p95`] = v.stage_us[s].p95;
        h[`seq.${shape}.${s}.p99`] = v.stage_us[s].p99;
      }
      h[`seq.${shape}.client_end_to_end.p50`] = v.stage_us.client_end_to_end.p50;
      h[`seq.${shape}.client_end_to_end.p95`] = v.stage_us.client_end_to_end.p95;
      h[`seq.${shape}.client_end_to_end.p99`] = v.stage_us.client_end_to_end.p99;
      h[`seq.${shape}.true_peak_rss_kb`] = v.rss_kb.true_peak;
      h[`seq.${shape}.baseline_rss_kb`] = v.rss_kb.baseline;
    }
    for (const [k, v] of Object.entries(run.held_request_memory)) {
      h[`held.${k}.reservation_bytes`] = v.reservation_bytes;
      h[`held.${k}.rss_growth_held_bytes`] = v.rss_growth_held_bytes;
      h[`held.${k}.true_peak_growth_bytes`] = v.true_peak_growth_bytes;
      h[`held.${k}.held_growth_over_reservation`] = v.held_growth_over_reservation;
      h[`held.${k}.peak_growth_over_reservation`] = v.peak_growth_over_reservation;
    }
    for (const key of ["mixed_overload", "mixed_ample"]) {
      const m = run[key];
      let total = 0;
      let ok = 0;
      let over = 0;
      let closed = 0;
      for (const t of Object.values(m.by_shape)) {
        total += t.requests;
        ok += t.status_counts[200] ?? 0;
        over += t.outcome_counts.overload ?? 0;
        closed += t.outcome_counts.connection_closed ?? 0;
      }
      h[`${key}.requests`] = total;
      h[`${key}.ok_200`] = ok;
      h[`${key}.overload_503`] = over;
      h[`${key}.connection_closed`] = closed;
      h[`${key}.sse_ok`] = m.sse_readers.status_counts[200] ?? 0;
      h[`${key}.sse_refused_503`] = m.sse_readers.status_counts[503] ?? 0;
      for (const k of ["inspection_in_use", "memory_units_in_use", "receipt_in_use", "upstream_in_use", "stream_in_use", "waiting", "threads", "sockets", "rss_kb"]) h[`${key}.peak.${k}`] = m.peaks_sampled[k];
      h[`${key}.true_peak_rss_kb`] = m.true_peak_rss.max_rss_kb;
      h[`${key}.baseline_rss_kb`] = m.baseline_rss_kb;
      h[`${key}.recovery.counters_zero_after_ms`] = m.recovery.counters_zero_after_ms;
      h[`${key}.recovery.sockets_back_after_ms`] = m.recovery.sockets_back_to_baseline_after_ms;
      h[`${key}.recovery.probe_ok_of_10`] = m.recovery.probe_requests_ok_of_10;
      h[`${key}.unexplained_provider_calls`] = m.invariants.provider_calls_not_explained_by_forwarded_requests;
      h[`${key}.secret_bodies_at_provider`] = m.invariants.provider_bodies_with_secret_prefix;
      for (const [s, v] of Object.entries(m.stage_under_load)) {
        h[`${key}.stage.${s}.p50`] = v.p50_us;
        h[`${key}.stage.${s}.p95`] = v.p95_us;
        h[`${key}.stage.${s}.p99`] = v.p99_us;
      }
    }
    for (const variant of ["mixed_large_and_small", "small_only_control"]) {
      const s = run.starvation[variant];
      h[`starvation.${variant}.small_ok`] = s.small.status_counts[200] ?? 0;
      h[`starvation.${variant}.small_total`] = s.small.requests;
      h[`starvation.${variant}.small_ok_p50_us`] = s.small.ok_latency_us.p50;
      h[`starvation.${variant}.small_ok_p99_us`] = s.small.ok_latency_us.p99;
      h[`starvation.${variant}.large_ok`] = s.large.status_counts[200] ?? 0;
      h[`starvation.${variant}.large_total`] = s.large.requests;
      h[`starvation.${variant}.admission_wait_p95_us`] = s.stage_under_load.admission_wait?.p95_us ?? null;
      h[`starvation.${variant}.admission_wait_p99_us`] = s.stage_under_load.admission_wait?.p99_us ?? null;
    }
    const c = run.cancellation_storm;
    h["cancel.requests"] = c.requests.requests;
    h["cancel.client_abort"] = c.requests.outcome_counts.client_abort ?? 0;
    h["cancel.inspection_in_use_peak"] = c.peaks_sampled.inspection_in_use;
    h["cancel.memory_units_in_use_peak"] = c.peaks_sampled.memory_units_in_use;
    h["cancel.counters_zero_after_ms"] = c.counters_zero_after_ms;
    h["cancel.inspections_awaited_to_completion"] = c.inspections_awaited_to_completion;
    h["cancel.worker_jobs_completed_approx"] = c.worker_jobs_completed_approx;
    h["cancel.provider_received_before_probes"] = c.provider_received_before_probes;
    h["cancel.true_peak_rss_kb"] = c.true_peak_rss.max_rss_kb;
    h["sockets.open_after_1s"] = run.sockets_only.open_after_1s;
    h["sockets.gateway_sockets_held"] = run.sockets_only.gateway_sockets_held;
    h["sockets.back_after_ms"] = run.sockets_only.sockets_back_after_ms;
    return h;
  });

  const version = execFileSync(BINARY, ["--version"], { encoding: "utf8" })
    .trim()
    .replace(/redact-secret-gateway-qualification/, "qualification binary")
    .replace(/\s*\[RSG-[^\]]*\]/, "");
  const report = {
    kind: "alpha2-aggregate-load",
    tool: "qualification/perf/run.mjs --aggregate (measurement tool; no performance claim)",
    quick_mode: QUICK,
    build: version,
    profile: process.env.QUAL_PROFILE ?? "unknown",
    source_commit: process.env.GATEWAY_COMMIT ?? null,
    host: {
      os: `${os.platform()} ${os.release()}`,
      arch: os.arch(),
      cpu_model: os.cpus()[0]?.model ?? null,
      cpus,
      memory_gib: Math.round(os.totalmem() / 2 ** 30),
      node: process.version,
      rustc: await rustcVersion(),
      load_threshold_load1: Math.round(cpus * QUIET_FRACTION * 100) / 100,
      quiet_wait_ms: QUIET_WAIT_MS,
    },
    runs_requested: RUNS,
    all_runs_quiet: runs.every((r) => !r.provisional),
    provisional: runs.some((r) => r.provisional),
    provisional_reason: runs.some((r) => r.provisional)
      ? `at least one run started or ended with the one-minute load average above ${Math.round(cpus * QUIET_FRACTION * 100) / 100} (25% of ${cpus} CPUs); runs above it are marked, and no figure here is a quiet-host measurement unless every run says so`
      : null,
    provider: "scripted fake provider on loopback (replies at once, after 150 ms, or holds the stream); not model delay",
    config_notes: {
      standard: "capacity receipt 16, memory_units 262144, inspection 4, upstream 16, stream 16, default limits",
      tight: "capacity receipt 8, memory_units 16384, inspection 4, upstream 8, stream 4; max_connections 64, admission_wait_ms 250, admission_queue 16",
      load,
    },
    percentile_note: "stage percentiles under load come from the gateway's log-bucketed histograms and are bucket upper edges (at most 25% above the true value); sequential per-request figures are exact deltas",
    median_and_spread: spread(headline),
    runs,
  };
  mkdirSync(EVIDENCE, { recursive: true });
  const outFile = path.join(EVIDENCE, QUICK ? "aggregate-load-quick.json" : "aggregate-load.json");
  writeFileSync(outFile, `${JSON.stringify(report, null, 2)}\n`);
  console.log(`wrote ${path.basename(outFile)} (runs ${RUNS}; provisional: ${report.provisional})`);
  rmSync(tmp, { recursive: true, force: true });
}

if (AGGREGATE) {
  await aggregateMain();
  process.exit(0);
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
