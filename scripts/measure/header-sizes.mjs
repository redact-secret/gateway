// Header-size measurement for #41 (ADR 0023). Outside the product binary and outside the
// qualification build seam: no gateway is involved. A raw TCP listener records the request
// head that the PINNED OpenAI SDKs (qualification/sdk lockfiles) put on the wire, for
// synthetic credentials only, and reports the quantities the gateway's caps are defined on.
//
// Usage (from the repository root, after the installs in scripts/measure/README.txt):
//   node scripts/measure/header-sizes.mjs [--json out.json]
//
// Intermediary headers (proxy, tracing, cookies) are *modelled*: they are added through the
// SDK's default-header option at documented typical sizes, which puts the same bytes in the
// head as a proxy inserting them would. Real intermediaries were not captured.
import { execFile, spawnSync } from "node:child_process";
import { promisify } from "node:util";
import { readFileSync, writeFileSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const nodeSdk = path.join(root, "qualification/sdk/node");
const pyBin = process.env.MEASURE_PYTHON ?? path.join(root, "qualification/sdk/python/.venv/bin/python");
const { default: OpenAI } = await import(pathToFileURL(path.join(nodeSdk, "node_modules/openai/index.mjs")).href);

// ---- synthetic inputs (never real credentials) -------------------------------------------
const key = (n) => ("sk-SYNTHETIC-NOT-A-KEY-" + "x".repeat(n)).slice(0, n);
const ORG = "org-SYNTHETICorg0123456789ab";
const PROJ = "proj_SYNTHETICproject01234567";
const UA_SUFFIX = " AcmeApp/4.2.1 (build 20260901; team=platform-ai; env=prod; region=eu-west-1)";
const ipv4 = (i) => `203.0.113.${(i % 250) + 1}`;
const ipv6 = (i) => `2001:db8:${(i + 1).toString(16)}:0:0:0:0:${(i + 7).toString(16)}`;
const hex = (n) => "0123456789abcdef".repeat(Math.ceil(n / 16)).slice(0, n);
const range = (n) => Array.from({ length: n }, (_, i) => i);

const proxyBasic = {
  Forwarded: "for=203.0.113.7;proto=https;host=api.example.test",
  "X-Forwarded-For": [1, 2, 3].map(ipv4).join(", "),
  "X-Forwarded-Proto": "https",
  "X-Forwarded-Host": "api.example.test",
  "X-Forwarded-Port": "443",
  "X-Real-IP": "203.0.113.7",
  Via: "1.1 edge-proxy-a, 1.1 corp-gateway-b",
  "X-Request-ID": hex(32),
};
const proxyLong = {
  Forwarded: range(10).map((i) => `for="[${ipv6(i)}]";proto=https`).join(", "),
  "X-Forwarded-For": range(10).map(ipv6).join(", "),
  "X-Forwarded-Proto": "https",
  "X-Forwarded-Host": "api.example.test",
  "X-Forwarded-Port": "443",
  "X-Real-IP": "203.0.113.7",
  Via: [1, 2, 3, 4, 5].map((i) => `1.1 hop-${i}.corp.example.test`).join(", "),
  "X-Request-ID": hex(32),
};
function baggageEntries(total) {
  const parts = [];
  let len = 0;
  for (let i = 0; len < total; i++) {
    const part = `k${i}=${hex(40)}`;
    const add = (parts.length ? 1 : 0) + part.length;
    if (len + add > total) {
      const rest = total - len - (parts.length ? 1 : 0);
      if (rest > 0) parts.push(`k${i}=`.padEnd(rest, "v").slice(0, rest));
      break;
    }
    parts.push(part);
    len += add;
  }
  return parts.join(",");
}
const trace = (baggageBytes, tracestateBytes) => ({
  traceparent: `00-${hex(32)}-${hex(16)}-01`,
  tracestate: ("vendor1=" + hex(32) + "," + range(31).map((i) => `v${i}=${hex(12)}`).join(",")).slice(0, tracestateBytes),
  baggage: baggageEntries(baggageBytes),
});
const cookies = (bytes) => {
  const parts = [];
  let len = 0;
  for (let i = 0; len < bytes; i++) {
    const c = `c${i}=${hex(Math.min(190, Math.max(1, bytes - len - 8)))}`;
    parts.push(c);
    len += c.length + 2;
  }
  return parts.join("; ").slice(0, bytes);
};

const SMALL_TRACE = trace(200, 200);
const cases = [
  { id: "baseline", note: "standard key, no optional headers", key: key(51) },
  { id: "org-project", note: "OpenAI-Organization + OpenAI-Project", key: key(51), org: ORG, project: PROJ },
  { id: "project-key", note: "164-byte project-scoped key shape", key: key(164), org: ORG, project: PROJ },
  { id: "token-512", note: "Authorization token at the 512-byte credential cap", key: key(512), org: ORG, project: PROJ },
  { id: "ua-suffix", note: "custom user-agent addition (+82 bytes)", key: key(164), org: ORG, project: PROJ, uaSuffix: UA_SUFFIX },
  { id: "proxy-basic", note: "Forwarded, X-Forwarded-*, X-Real-IP, Via, request id", key: key(164), org: ORG, project: PROJ, extra: proxyBasic },
  { id: "proxy-long-chain", note: "10-hop IPv6 chains in Forwarded and X-Forwarded-For, 5 Via hops", key: key(164), org: ORG, project: PROJ, extra: proxyLong },
  { id: "tracing-small", note: "traceparent, tracestate 200 B, baggage 200 B", key: key(164), org: ORG, project: PROJ, extra: SMALL_TRACE },
  { id: "tracing-baggage-1k", note: "tracestate 512 B, baggage 1 KiB", key: key(164), org: ORG, project: PROJ, extra: trace(1024, 512) },
  { id: "tracing-baggage-8192", note: "baggage at the W3C 8192-byte limit, tracestate 512 B", key: key(164), org: ORG, project: PROJ, extra: trace(8192, 512) },
  { id: "cookie-1k", note: "enterprise proxy cookies, 1 KiB", key: key(164), org: ORG, project: PROJ, extra: { Cookie: cookies(1024) } },
  { id: "cookie-4k", note: "Cookie header of 4 KiB", key: key(164), org: ORG, project: PROJ, extra: { Cookie: cookies(4096) } },
  { id: "cookie-8192", note: "Cookie header value of 8192 bytes", key: key(164), org: ORG, project: PROJ, extra: { Cookie: cookies(8192) } },
  { id: "combined-typical", note: "project key, org/project, UA, proxy chain, small tracing, 1 KiB cookies", key: key(164), org: ORG, project: PROJ, uaSuffix: UA_SUFFIX, extra: { ...proxyBasic, ...SMALL_TRACE, Cookie: cookies(1024) } },
  { id: "combined-heavy", note: "512 B token, UA, long proxy chains, 1 KiB baggage, 4 KiB cookies", key: key(512), org: ORG, project: PROJ, uaSuffix: UA_SUFFIX, extra: { ...proxyLong, ...trace(1024, 512), Cookie: cookies(4096) } },
  { id: "combined-extreme", note: "heavy case with 8192-byte baggage (W3C max) and 4 KiB cookies", key: key(512), org: ORG, project: PROJ, uaSuffix: UA_SUFFIX, extra: { ...proxyLong, ...trace(8192, 512), Cookie: cookies(4096) } },
];

// ---- raw listener ------------------------------------------------------------------------
const heads = [];
const server = net.createServer((sock) => {
  let buf = Buffer.alloc(0);
  let done = false;
  sock.on("data", (d) => {
    if (done) return;
    buf = Buffer.concat([buf, d]);
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0) return;
    const head = buf.subarray(0, end + 4);
    const m = /\r\ncontent-length:\s*(\d+)/i.exec(head.toString("latin1"));
    const need = end + 4 + (m ? Number(m[1]) : 0);
    if (buf.length < need) return;
    done = true;
    heads.push(head);
    const body = JSON.stringify({ id: "x", object: "chat.completion", created: 0, model: "m", choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }] });
    sock.end(`HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n${body}`);
  });
  sock.on("error", () => {});
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const baseURL = `http://127.0.0.1:${server.address().port}/v1`;

// The quantities the gateway's limits are defined on: `nameValueBytes` is what the route's
// total cap counts (sum of name and value lengths, no separators); `headBytes` is what the
// head guard's hold bound counts (request line through the blank line).
function analyze(head) {
  const lines = head.toString("latin1").slice(0, -4).split("\r\n");
  const requestLine = lines.shift();
  let nv = 0, max = 0, maxName = "", authLen = 0, ua = null;
  for (const line of lines) {
    const i = line.indexOf(":");
    const name = line.slice(0, i);
    const value = line.slice(i + 1).replace(/^[ \t]+|[ \t]+$/g, "");
    nv += name.length + value.length;
    if (value.length > max) { max = value.length; maxName = name.toLowerCase(); }
    if (name.toLowerCase() === "authorization") authLen = value.length - "Bearer ".length;
    if (name.toLowerCase() === "user-agent") ua = value;
  }
  return { headBytes: head.length, requestLineBytes: requestLine.length + 2, headerCount: lines.length, nameValueBytes: nv, maxValueBytes: max, maxValueName: maxName, tokenBytes: authLen, userAgent: ua };
}

const run = promisify(execFile);
const results = [];
const chat = { model: "m", messages: [{ role: "user", content: "hi" }] };
let nodeUA = "OpenAI/JS 7.27.0";
for (const c of cases) {
  const headers = { ...(c.extra ?? {}) };
  if (c.uaSuffix) headers["User-Agent"] = nodeUA + c.uaSuffix;
  const client = new OpenAI({ apiKey: c.key, baseURL, organization: c.org ?? null, project: c.project ?? null, maxRetries: 0, defaultHeaders: headers });
  const before = heads.length;
  await client.chat.completions.create(chat);
  if (heads.length !== before + 1) throw new Error(`node ${c.id}: expected one request`);
  const a = analyze(heads[heads.length - 1]);
  if (c.id === "baseline") nodeUA = a.userAgent;
  results.push({ sdk: "node", id: c.id, note: c.note, ...a });

  const before2 = heads.length;
  // Asynchronous on purpose: the listener runs in this process and must keep serving.
  try {
    await run(pyBin, [path.join(here, "header-sizes-python.py"), baseURL, JSON.stringify(c)]);
  } catch (e) {
    throw new Error(`python ${c.id}: ${e.stderr ?? e.message}`);
  }
  if (heads.length !== before2 + 1) throw new Error(`python ${c.id}: expected one request`);
  results.push({ sdk: "python", id: c.id, note: c.note, ...analyze(heads[heads.length - 1]) });
}
server.close();

const py = spawnSync(pyBin, ["-c", "import openai,platform;print(openai.__version__, platform.python_version())"], { encoding: "utf8" }).stdout.trim();
const meta = {
  os: `${os.type()} ${os.release()} ${os.arch()}`,
  node: process.version,
  node_sdk: JSON.parse(readFileSync(path.join(nodeSdk, "node_modules/openai/package.json"), "utf8")).version,
  python_sdk_and_python: py,
  note: "synthetic credentials only; intermediary headers are modelled, not captured from real proxies",
};
const ix = process.argv.indexOf("--json");
if (ix > 0) writeFileSync(process.argv[ix + 1], JSON.stringify({ meta, results }, null, 2) + "\n");
console.log(JSON.stringify(meta));
console.log("| SDK | case | fields | head bytes | name+value bytes | largest value | largest field |");
console.log("| --- | --- | ---: | ---: | ---: | ---: | --- |");
for (const r of results) console.log(`| ${r.sdk} | ${r.id} | ${r.headerCount} | ${r.headBytes} | ${r.nameValueBytes} | ${r.maxValueBytes} | ${r.maxValueName} |`);
