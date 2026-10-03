// Scripted FAKE provider for the SDK qualification (ADR 0020). Synthetic data only.
//
// Plays the part of the OpenAI Chat Completions endpoint behind the NON-RELEASE
// qualification gateway build. It is test tooling: it listens on loopback only, holds no
// credential, and invents every byte it returns. No dependencies (Node built-ins only).
//
//   node server.mjs            -> prints one JSON line {"provider_port":N,"admin_port":M}
//
// The scenario is chosen by the request `model` (a transmitted-verbatim structural field), so
// it survives the gateway's sanitizing rewrite and SDK retries hit the same script:
//   qual-example (json-ok, or sse-ok when stream:true), qual-json-ok, qual-json-slow (replies after 150 ms), qual-json-4mib (a 4 MB reply, just under the default response bound; #58 load runs), qual-json-large, qual-json-oversize, qual-json-truncated, qual-hang,
//   qual-err-<status> (400 401 403 404 408 409 422 429 500 502 503 504),
//   qual-err-429-retry-after (Retry-After: 0), qual-err-500-hint-no-retry (500 + x-should-retry: false), qual-retry-429-then-ok (two 429s, then 200),
//   qual-sse-ok, qual-sse-fragmented, qual-sse-multi, qual-sse-interrupted(-close), qual-sse-gated,
//   qual-sse-hang, qual-sse-slow, qual-sse-err-429.
// Framing and delivery-uncertainty scenarios (#60), all raw bytes on the provider socket:
//   qual-close-before-headers, qual-partial-head, qual-bad-status-line, qual-dup-content-length,
//   qual-gzip (Content-Encoding: gzip), qual-partial-read (provider stops reading the request body),
//   qual-sse-cut-before-first-event, qual-sse-clean-no-finish (clean end, no finish_reason, no [DONE]),
//   qual-sse-no-done (finish_reason but no [DONE], clean end), qual-sse-bad-chunk (invalid chunk framing).
//
// Admin API (separate loopback port; the gateway never talks to it):
//   GET  /__admin/calls                      -> {connections, calls:[...]} (recorded calls)
//   POST /__admin/reset                      -> forget calls, release gates
//   GET  /__admin/await?call=N&event=E&timeout_ms=T   E: received|first_event|gated|closed|finished
//   GET  /__admin/await-calls?count=N&timeout_ms=T    resolves when >= N calls were received
//   POST /__admin/release?call=N             -> let a gated stream continue
//   GET  /__admin/stats                      -> {received, secret_bodies, bytes}: counts that survive keep_bodies=0
//   POST /__admin/mode?keep_bodies=0|1       -> 0 keeps no request body text (aggregate load runs, #58)
// Waiting is event-driven (no polling, no sleeping): a deadline only turns a hang into a failure.

import http from "node:http";
import crypto from "node:crypto";

const MAX_BODY = 8 * 1024 * 1024;
const PIECES = ["Hel", "lo ", "안녕", "하세요", " 🙂", "!"];
const T0 = process.hrtime.bigint();
const nowMs = () => Number((process.hrtime.bigint() - T0) / 1000n) / 1000;

/** @type {Array<any>} */
let calls = [];
let keepBodies = true;
let stats = { received: 0, secret_bodies: 0, bytes: 0 };
let connections = 0;
let scenarioCounts = new Map();
let waiters = []; // {test: () => boolean, resolve}
let gates = new Map(); // seq -> resolve fn

function notify() {
  const pending = waiters;
  waiters = [];
  for (const w of pending) {
    if (w.test()) w.resolve(true);
    else waiters.push(w);
  }
}

function waitFor(test, timeoutMs) {
  if (test()) return Promise.resolve(true);
  return new Promise((resolve) => {
    const w = { test, resolve };
    waiters.push(w);
    setTimeout(() => {
      const i = waiters.indexOf(w);
      if (i >= 0) {
        waiters.splice(i, 1);
        resolve(false);
      }
    }, timeoutMs).unref();
  });
}

function mark(call, event) {
  if (!call.events.includes(event)) call.events.push(event);
  call.times[event] = nowMs();
  notify();
}

function event(i, delta, finish) {
  return Buffer.from(
    `data: ${JSON.stringify({
      id: "chatcmpl-qual-stream",
      object: "chat.completion.chunk",
      created: 1700000000,
      model: "qual",
      choices: [{ index: 0, delta, finish_reason: finish ?? null }],
    })}\n\n`,
    "utf8",
  );
}

function streamEvents() {
  const out = [event(0, { role: "assistant", content: "" })];
  PIECES.forEach((p, i) => out.push(event(i + 1, { content: p })));
  out.push(event(99, {}, "stop"));
  out.push(Buffer.from("data: [DONE]\n\n", "utf8"));
  return out;
}

const tick = () => new Promise((r) => setImmediate(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function completion(seq, text) {
  return {
    id: `chatcmpl-qual-${seq}`,
    object: "chat.completion",
    created: 1700000000,
    model: "qual",
    choices: [{ index: 0, message: { role: "assistant", content: text }, finish_reason: "stop" }],
    usage: { prompt_tokens: 3, completion_tokens: 5, total_tokens: 8 },
  };
}

function sendJson(res, status, obj, extra = {}) {
  const body = Buffer.from(JSON.stringify(obj), "utf8");
  res.writeHead(status, {
    "content-type": "application/json",
    "content-length": body.length,
    "x-request-id": `req_qual_${calls.length}`,
    ...extra,
  });
  res.end(body);
}

function providerError(status) {
  return {
    error: {
      message: `synthetic provider error ${status}`,
      type: "synthetic_error",
      param: null,
      code: `synthetic_${status}`,
    },
  };
}

function beginSse(res, call, extra = {}) {
  res.socket?.setNoDelay(true);
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    "x-request-id": `req_qual_${call.seq}`,
    ...extra,
  });
  res.flushHeaders();
}

const ERR_STATUSES = new Set([400, 401, 403, 404, 408, 409, 422, 429, 500, 502, 503, 504]);

async function runScenario(call, req, res, json) {
  const streaming = json?.stream === true;
  let model = typeof json?.model === "string" ? json.model : "";
  // `qual-example` serves the verified examples: a normal reply, or an event stream when asked.
  if (model === "qual-example") model = streaming ? "qual-sse-ok" : "qual-json-ok";
  const attempt = (scenarioCounts.get(model) ?? 0) + 1;
  scenarioCounts.set(model, attempt);
  const alive = () => !call.events.includes("closed");

  if (model === "qual-json-ok") {
    return sendJson(res, 200, completion(call.seq, PIECES.join("")));
  }
  if (model === "qual-json-slow") {
    await sleep(150);
    if (!alive()) return;
    return sendJson(res, 200, completion(call.seq, PIECES.join("")));
  }
  if (model === "qual-json-4mib") {
    return sendJson(res, 200, completion(call.seq, "x".repeat(4_000_000)));
  }
  if (model === "qual-json-large") {
    return sendJson(res, 200, completion(call.seq, "synthetic large reply ".repeat(3000)));
  }
  if (model === "qual-json-oversize") {
    // Over the gateway's response-body bound (4 MiB by default): refused, never relayed.
    return sendJson(res, 200, completion(call.seq, "x".repeat(5 * 1024 * 1024)));
  }
  if (model === "qual-json-truncated") {
    res.writeHead(200, { "content-type": "application/json", "content-length": 5000 });
    res.write('{"id":"chatcmpl-qual-trunc"');
    await tick();
    call.server_aborted = true;
    res.socket?.destroy();
    return;
  }
  if (model === "qual-close-before-headers") {
    call.server_aborted = true;
    res.socket?.destroy();
    return;
  }
  if (model === "qual-partial-head" || model === "qual-bad-status-line" || model === "qual-dup-content-length" || model === "qual-gzip") {
    const raw = {
      "qual-partial-head": "HTTP/1.1 200 OK\r\nContent-Ty",
      "qual-bad-status-line": "ICY 200 OK\r\nContent-Length: 2\r\n\r\n{}",
      "qual-dup-content-length": "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{}x",
      "qual-gzip": "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    }[model];
    call.server_aborted = true;
    res.socket?.write(raw, () => res.socket?.destroy());
    return;
  }
  if (model === "qual-hang") {
    return; // never answer; ends when the client (via the gateway) goes away
  }
  if (model === "qual-retry-429-then-ok") {
    if (attempt <= 2) return sendJson(res, 429, providerError(429));
    return sendJson(res, 200, completion(call.seq, PIECES.join("")));
  }
  if (model === "qual-err-429-retry-after") {
    return sendJson(res, 429, providerError(429), { "retry-after": "0" });
  }
  if (model === "qual-err-500-hint-no-retry") {
    // Provider retry hints the OpenAI SDKs obey; the gateway's response-header allowlist drops them.
    return sendJson(res, 500, providerError(500), { "x-should-retry": "false", "retry-after-ms": "10" });
  }
  const m = /^qual-err-(\d{3})$/.exec(model);
  if (m && ERR_STATUSES.has(Number(m[1]))) {
    return sendJson(res, Number(m[1]), providerError(Number(m[1])));
  }
  if (model === "qual-sse-err-429" && streaming) {
    return sendJson(res, 429, providerError(429));
  }

  if (streaming && model.startsWith("qual-sse-")) {
    const events = streamEvents();
    switch (model) {
      case "qual-sse-ok": {
        beginSse(res, call);
        for (const [i, e] of events.entries()) {
          res.write(e);
          if (i === 0) mark(call, "first_event");
          await tick();
        }
        return res.end();
      }
      case "qual-sse-fragmented": {
        beginSse(res, call);
        const all = Buffer.concat(events);
        const sizes = [1, 2, 3, 1, 5, 2, 7, 4];
        let at = 0;
        let i = 0;
        while (at < all.length && alive()) {
          const n = sizes[i++ % sizes.length];
          res.write(all.subarray(at, at + n));
          if (at === 0) mark(call, "first_event");
          at += n;
          await tick();
        }
        return res.end();
      }
      case "qual-sse-multi": {
        beginSse(res, call);
        for (let i = 0; i < events.length; i += 3) {
          res.write(Buffer.concat(events.slice(i, i + 3)));
          if (i === 0) mark(call, "first_event");
          await tick();
        }
        return res.end();
      }
      case "qual-sse-interrupted":
      case "qual-sse-interrupted-close": {
        // The -close variant announces `Connection: close` like the gateway does (control for
        // the Node.js fetch observation in docs/contracts/errors-and-telemetry.md).
        beginSse(res, call, model.endsWith("-close") ? { connection: "close" } : {});
        res.write(events[0]);
        await new Promise((r) => res.write(events[1], r)); // both events handed to the kernel
        mark(call, "first_event");
        call.server_aborted = true;
        res.socket?.destroy(); // no terminating chunk, no [DONE]
        return;
      }
      case "qual-sse-cut-before-first-event": {
        beginSse(res, call);
        // Give the gateway time to read the headers and commit the response; a provider that
        // closes in the same instant can be observed as a pre-commit 502 instead (a race the
        // gateway cannot and need not resolve; documented in the contract).
        await sleep(150);
        call.server_aborted = true;
        res.socket?.destroy(); // headers sent, then nothing: no event, no terminating chunk
        return;
      }
      case "qual-sse-clean-no-finish": {
        // A provider that ends the stream cleanly (terminating chunk) after only two events:
        // no finish_reason and no [DONE]. The transport is valid; the completion is not.
        beginSse(res, call);
        res.write(events[0]);
        res.write(events[1]);
        mark(call, "first_event");
        return res.end();
      }
      case "qual-sse-no-done": {
        beginSse(res, call);
        for (const e of events.slice(0, -1)) {
          res.write(e);
          await tick();
        }
        mark(call, "first_event");
        return res.end(); // finish_reason arrived, [DONE] did not
      }
      case "qual-sse-bad-chunk": {
        // Invalid chunk framing after the headers: the size line is not hexadecimal.
        beginSse(res, call);
        res.write(events[0]);
        await new Promise((r) => res.socket?.write("ZZ\r\ngarbage\r\n", r));
        mark(call, "first_event");
        call.server_aborted = true;
        res.socket?.destroy();
        return;
      }
      case "qual-sse-gated": {
        beginSse(res, call);
        res.write(events[0]);
        res.write(events[1]);
        mark(call, "first_event");
        const released = new Promise((resolve) => gates.set(call.seq, resolve));
        mark(call, "gated");
        await Promise.race([released, waitFor(() => !alive(), 60_000)]);
        for (const e of events.slice(2)) {
          if (!alive()) return;
          res.write(e);
          await tick();
        }
        return res.end();
      }
      case "qual-sse-hang": {
        beginSse(res, call);
        res.write(events[0]);
        res.write(events[1]);
        mark(call, "first_event");
        return; // hold the stream open until the caller disconnects
      }
      case "qual-sse-slow": {
        beginSse(res, call);
        for (const [i, e] of events.entries()) {
          if (!alive()) return;
          res.write(e);
          if (i === 0) mark(call, "first_event");
          await sleep(25);
        }
        return res.end();
      }
      default:
    }
  }
  return sendJson(res, 404, providerError(404));
}

const provider = http.createServer((req, res) => {
  const call = {
    seq: calls.length + 1,
    method: req.method,
    path: req.url,
    header_names: Object.keys(req.headers),
    has_authorization: typeof req.headers.authorization === "string",
    authorization_sha256: req.headers.authorization
      ? crypto.createHash("sha256").update(req.headers.authorization).digest("hex")
      : null,
    content_type: req.headers["content-type"] ?? null,
    user_agent: req.headers["user-agent"] ?? null,
    body: "",
    body_bytes: 0,
    scenario: "",
    events: [],
    times: {},
    server_aborted: false,
  };
  calls.push(call);
  const chunks = [];
  let size = 0;
  let partial = false;
  req.on("data", (c) => {
    size += c.length;
    if (size <= MAX_BODY) chunks.push(c);
    // `qual-partial-read`: stop reading the request body after the first chunk that names the
    // scenario and close the connection (a provider that dies mid-upload).
    if (!partial && c.includes("qual-partial-read")) {
      partial = true;
      call.body_bytes = size;
      call.body = Buffer.concat(chunks).toString("utf8");
      call.scenario = "qual-partial-read";
      call.server_aborted = true;
      mark(call, "received");
      req.socket.destroy();
    }
  });
  req.socket.on("close", () => {
    if (!res.writableFinished && !call.server_aborted) mark(call, "closed");
    else if (call.server_aborted) mark(call, "aborted_by_provider");
  });
  res.on("finish", () => mark(call, "finished"));
  req.on("end", () => {
    if (partial) return;
    const raw = Buffer.concat(chunks);
    stats.received += 1;
    stats.bytes += size;
    if (raw.includes("ghp_SYNTH")) stats.secret_bodies += 1;
    call.body = keepBodies ? raw.toString("utf8") : "";
    const text = keepBodies ? call.body : raw.toString("utf8");
    call.body_bytes = size;
    let json = null;
    try {
      json = JSON.parse(text);
    } catch {
      /* not JSON: recorded as is */
    }
    call.scenario = typeof json?.model === "string" ? json.model : "";
    mark(call, "received");
    if (req.method !== "POST" || req.url !== "/v1/chat/completions") {
      return sendJson(res, 404, providerError(404));
    }
    runScenario(call, req, res, json).catch(() => {
      res.socket?.destroy();
    });
  });
});
provider.on("connection", (s) => {
  connections += 1;
  s.setNoDelay(true);
  notify();
});

const admin = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const out = (status, obj) => {
    const body = JSON.stringify(obj);
    res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
    res.end(body);
  };
  if (url.pathname === "/__admin/health") return out(200, { ok: true });
  if (url.pathname === "/__admin/calls") return out(200, { connections, calls });
  if (url.pathname === "/__admin/stats") return out(200, { ...stats, connections });
  if (url.pathname === "/__admin/mode" && req.method === "POST") {
    keepBodies = url.searchParams.get("keep_bodies") !== "0";
    return out(200, { ok: true, keep_bodies: keepBodies });
  }
  if (url.pathname === "/__admin/reset" && req.method === "POST") {
    for (const release of gates.values()) release();
    gates = new Map();
    calls = [];
    stats = { received: 0, secret_bodies: 0, bytes: 0 };
    connections = 0;
    scenarioCounts = new Map();
    return out(200, { ok: true });
  }
  if (url.pathname === "/__admin/release" && req.method === "POST") {
    const release = gates.get(Number(url.searchParams.get("call")));
    if (!release) return out(404, { ok: false });
    release();
    return out(200, { ok: true });
  }
  const timeout = Number(url.searchParams.get("timeout_ms") ?? "10000");
  if (url.pathname === "/__admin/await") {
    const seq = Number(url.searchParams.get("call"));
    const ev = url.searchParams.get("event") ?? "";
    const ok = await waitFor(() => calls.find((c) => c.seq === seq)?.events.includes(ev) === true, timeout);
    return out(ok ? 200 : 408, { ok });
  }
  if (url.pathname === "/__admin/await-calls") {
    const count = Number(url.searchParams.get("count"));
    const ok = await waitFor(() => calls.filter((c) => c.events.includes("received")).length >= count, timeout);
    return out(ok ? 200 : 408, { ok, received: calls.filter((c) => c.events.includes("received")).length });
  }
  return out(404, { ok: false });
});

function listen(server) {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));
}

const providerPort = await listen(provider);
const adminPort = await listen(admin);
process.stdout.write(`${JSON.stringify({ provider_port: providerPort, admin_port: adminPort })}\n`);
for (const sig of ["SIGTERM", "SIGINT"]) {
  process.on(sig, () => process.exit(0));
}
