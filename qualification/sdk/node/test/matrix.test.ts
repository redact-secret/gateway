// Alpha 2 transport qualification matrix (#60), Node SDK. Every scenario runs twice: once with the
// SDK's default retries (maxRetries = 2, "what an unconfigured app gets") and once with retries
// explicitly disabled (maxRetries: 0). For each run the matrix records
//   - the HTTP attempts the SDK made to the gateway,
//   - the requests and request bodies that reached the (fake) provider,
//   - whether those bodies were byte-identical (a client retry is a NEW request, so identical bodies
//     are duplicate provider-side work, not a gateway replay: the gateway never retries),
//   - for streams: how many events arrived, whether `finish_reason` was seen, and whether the SDK
//     raised on the way.
// The completion verdict a caller can rely on is `finish_reason` seen, never "the iteration ended".
// The assertions pin the runtime-independent facts; the runtime-dependent ones (does a cut stream
// raise on this Node.js?) are recorded for the runtime that ran and asserted only for the
// runtimes whose behavior was observed (docs/contracts/errors-and-telemetry.md).
import assert from "node:assert/strict";
import { after, describe, it } from "node:test";
import OpenAI from "openai";
import { closedPort, gateways, provider, synthetic, writeEvidence } from "./support.ts";

const msg = [{ role: "user" as const, content: "matrix observation" }];

interface Run {
  scenario: string;
  sdk_max_retries: "default(2)" | 0;
  sdk_attempts: number;
  sdk_outcome: string;
  provider_requests: number;
  provider_bodies_received: number;
  provider_bodies_identical: boolean;
  provider_body_bytes_received: number[];
  stream: null | { events: number; finish_reason_seen: boolean; sdk_raised: boolean; verdict: "complete" | "incomplete" };
}

const runs: Run[] = [];

// Observed in #60 (docs/contracts/errors-and-telemetry.md): the gateway ends every broken stream
// without the terminating chunk and writes nothing into it. Whether Node's fetch reports that cut
// depends on the runtime: Node.js 24 (undici 7.x) raised in every cut case, Node.js 22.16.0
// (undici 6.21.2) never did and the SDK iteration simply ended. Pinned only for those two; any
// other runtime is recorded, not asserted (no claim across untested versions).
function expectedRaiseOnCut(): boolean | null {
  const [major] = process.versions.node.split(".").map(Number);
  if ((major ?? 0) >= 24) return true;
  if (process.versions.node === "22.16.0") return false;
  return null;
}

async function run(
  scenario: string,
  gateway: string,
  model: string,
  stream: boolean,
  maxRetries: "default" | 0,
  content = "matrix observation",
): Promise<Run> {
  await provider.reset();
  let attempts = 0;
  const options: ConstructorParameters<typeof OpenAI>[0] = {
    baseURL: `${gateway}/v1`,
    apiKey: synthetic.api_key,
    fetch: async (input, init) => {
      attempts += 1;
      return fetch(input, init);
    },
  };
  if (maxRetries === 0) options.maxRetries = 0;
  const c = new OpenAI(options);
  let outcome = "ok";
  let streamInfo: Run["stream"] = null;
  try {
    if (stream) {
      const s = await c.chat.completions.create({ model, messages: [{ role: "user", content }], stream: true });
      let events = 0;
      let finish = false;
      let raised = false;
      try {
        for await (const chunk of s) {
          events += 1;
          if (chunk.choices[0]?.finish_reason) finish = true;
        }
      } catch {
        raised = true;
        outcome = "stream error";
      }
      streamInfo = { events, finish_reason_seen: finish, sdk_raised: raised, verdict: finish ? "complete" : "incomplete" };
    } else {
      await c.chat.completions.create({ model, messages: [{ role: "user", content }] });
    }
  } catch (e) {
    outcome = e instanceof OpenAI.APIError && e.status ? `status ${e.status}` : `error ${(e as Error).constructor.name}`;
  }
  const seen = await provider.calls();
  const bodies = seen.calls.filter((call) => call.body_bytes > 0).map((call) => call.body);
  const record: Run = {
    scenario,
    sdk_max_retries: maxRetries === 0 ? 0 : "default(2)",
    sdk_attempts: attempts,
    sdk_outcome: outcome,
    provider_requests: seen.calls.length,
    provider_bodies_received: bodies.length,
    provider_bodies_identical: bodies.length > 0 && bodies.every((b) => b === bodies[0]),
    provider_body_bytes_received: seen.calls.map((call) => call.body_bytes),
    stream: streamInfo,
  };
  runs.push(record);
  return record;
}

// Provider-side failures that become a gateway 5xx: each SDK attempt reaches the provider, so the
// default policy multiplies provider-side work by three while maxRetries: 0 sends exactly one.
const duplicating: Array<[string, string, string]> = [
  ["provider closes before any response byte", "qual-close-before-headers", "502 upstream_invalid_response"],
  ["provider sends a partial response head, then closes", "qual-partial-head", "502 upstream_invalid_response"],
  ["provider answers with a non-HTTP status line", "qual-bad-status-line", "502 upstream_invalid_response"],
  ["provider sends two different Content-Length values", "qual-dup-content-length", "502 upstream_invalid_response"],
  ["provider applies Content-Encoding: gzip", "qual-gzip", "502 upstream_invalid_response"],
  ["provider truncates the JSON body", "qual-json-truncated", "502 upstream_invalid_response"],
  ["provider response over the body bound", "qual-json-oversize", "502 upstream_response_too_large"],
  ["provider stops reading the request body (partial upstream send)", "qual-partial-read", "502 upstream_invalid_response"],
];

describe("Alpha 2 transport matrix (Node SDK)", () => {
  after(() => {
    writeEvidence("matrix-observations-node.json", {
      sdk: "openai (npm) 7.27.0",
      node: process.version,
      undici: process.versions.undici,
      note:
        "provider_requests counts what reached the fake provider; a client retry is a new request, so equal counts with identical bodies are duplicate provider-side work. The gateway never retries.",
      runs,
    });
  });

  for (const [label, model, mapped] of duplicating) {
    it(`${label} -> ${mapped}; default retries duplicate provider work, maxRetries 0 does not`, async () => {
      // The partial-send scenario uploads about 200 KB so the provider stops reading part way.
      const content = model === "qual-partial-read" ? "partial upstream send ".repeat(9000) : undefined;
      const withDefault = await run(label, gateways.standard, model, false, "default", content);
      const noRetry = await run(label, gateways.standard, model, false, 0, content);
      if (content !== undefined) {
        const full = JSON.stringify({ content }).length;
        assert.ok(
          withDefault.provider_body_bytes_received.every((n) => n > 0 && n < full),
          "the provider read only part of the request body",
        );
      }
      assert.equal(noRetry.sdk_attempts, 1);
      assert.equal(noRetry.provider_requests, 1, "exactly one request reached the provider");
      assert.equal(withDefault.sdk_attempts, 3);
      assert.equal(withDefault.provider_requests, 3, "delivery uncertainty: every retry reached the provider again");
      if (content === undefined) {
        assert.equal(withDefault.provider_bodies_identical, true, "the same sanitized body, sent three times as three requests");
      }
      assert.equal(withDefault.sdk_outcome, "status 502");
      assert.equal(noRetry.sdk_outcome, "status 502");
    });
  }

  it("provider never answers -> 504 upstream_timeout; default retries triple the provider work", async () => {
    const withDefault = await run("provider never answers", gateways.tight, "qual-hang", false, "default");
    const noRetry = await run("provider never answers", gateways.tight, "qual-hang", false, 0);
    assert.deepEqual([withDefault.sdk_attempts, withDefault.provider_requests], [3, 3]);
    assert.deepEqual([noRetry.sdk_attempts, noRetry.provider_requests], [1, 1]);
    assert.equal(withDefault.sdk_outcome, "status 504");
  });

  it("gateway overload -> 503 with Retry-After: retries wait and never reach the provider", async () => {
    await provider.reset();
    const holder = await new OpenAI({ baseURL: `${gateways.overload}/v1`, apiKey: synthetic.api_key, maxRetries: 0 }).chat.completions.create({
      model: "qual-sse-hang",
      messages: msg,
      stream: true,
    });
    const iterator = holder[Symbol.asyncIterator]();
    await iterator.next();
    const noRetry = await run("gateway overload", gateways.overload, "qual-json-ok", false, 0);
    const withDefault = await run("gateway overload", gateways.overload, "qual-json-ok", false, "default");
    holder.controller.abort();
    await iterator.next().then(() => undefined, () => undefined);
    assert.deepEqual([noRetry.sdk_attempts, noRetry.provider_requests], [1, 0]);
    assert.deepEqual([withDefault.sdk_attempts, withDefault.provider_requests], [3, 0]);
    assert.equal(withDefault.sdk_outcome, "status 503");
  });

  it("disconnect before the gateway answers (connection refused) -> retried by default, nothing reaches the provider", async () => {
    const base = `http://127.0.0.1:${await closedPort()}`;
    const withDefault = await run("gateway connection refused", base, "qual-json-ok", false, "default");
    const noRetry = await run("gateway connection refused", base, "qual-json-ok", false, 0);
    assert.deepEqual([withDefault.sdk_attempts, withDefault.provider_requests], [3, 0]);
    assert.deepEqual([noRetry.sdk_attempts, noRetry.provider_requests], [1, 0]);
    assert.equal(withDefault.sdk_outcome, "error APIConnectionError");
  });

  // Streams. Before the headers are committed the failure is an ordinary gateway status (retried by
  // default, duplicate provider work); after the headers it is a truncation the SDK never retries.
  it("stream: provider closes before any response byte -> 502 before the headers, retried by default", async () => {
    const withDefault = await run("stream, provider closes before headers", gateways.standard, "qual-close-before-headers", true, "default");
    const noRetry = await run("stream, provider closes before headers", gateways.standard, "qual-close-before-headers", true, 0);
    assert.deepEqual([withDefault.sdk_attempts, withDefault.provider_requests], [3, 3]);
    assert.deepEqual([noRetry.sdk_attempts, noRetry.provider_requests], [1, 1]);
    assert.equal(withDefault.sdk_outcome, "status 502");
  });

  type StreamCase = { label: string; model: string; gateway: string; finish: boolean; raisedEverywhere: boolean | null };
  const streams: StreamCase[] = [
    { label: "complete stream", model: "qual-sse-ok", gateway: gateways.standard, finish: true, raisedEverywhere: false },
    { label: "finish_reason but no [DONE], clean end", model: "qual-sse-no-done", gateway: gateways.standard, finish: true, raisedEverywhere: false },
    { label: "clean end after two events: no finish_reason, no [DONE]", model: "qual-sse-clean-no-finish", gateway: gateways.standard, finish: false, raisedEverywhere: false },
    { label: "provider cut after two events", model: "qual-sse-interrupted", gateway: gateways.standard, finish: false, raisedEverywhere: null },
    { label: "provider cut after the headers, before any event", model: "qual-sse-cut-before-first-event", gateway: gateways.standard, finish: false, raisedEverywhere: null },
    { label: "invalid chunk framing after the headers", model: "qual-sse-bad-chunk", gateway: gateways.standard, finish: false, raisedEverywhere: null },
    { label: "stalled provider stream cut by the gateway idle deadline", model: "qual-sse-hang", gateway: gateways.tight, finish: false, raisedEverywhere: null },
  ];
  for (const s of streams) {
    it(`stream: ${s.label}: one request, never retried, verdict ${s.finish ? "complete" : "incomplete"}`, async () => {
      const withDefault = await run(`stream: ${s.label}`, s.gateway, s.model, true, "default");
      const noRetry = await run(`stream: ${s.label}`, s.gateway, s.model, true, 0);
      for (const r of [withDefault, noRetry]) {
        assert.equal(r.sdk_attempts, 1, "the SDK never retries once the headers are committed");
        assert.equal(r.provider_requests, 1);
        assert.equal(r.stream?.finish_reason_seen, s.finish);
        assert.equal(r.stream?.verdict, s.finish ? "complete" : "incomplete");
        const expected = s.raisedEverywhere ?? expectedRaiseOnCut();
        if (expected !== null) assert.equal(r.stream?.sdk_raised, expected);
      }
    });
  }

  it("a clean provider end without finish_reason is NOT an error to the SDK: iteration ending is not completion", async () => {
    const r = await run("clean end without finish_reason", gateways.standard, "qual-sse-clean-no-finish", true, 0);
    assert.equal(r.stream?.sdk_raised, false);
    assert.equal(r.stream?.finish_reason_seen, false);
    assert.ok((r.stream?.events ?? 0) < 8);
  });
});
