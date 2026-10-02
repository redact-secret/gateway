// OBSERVED retry behavior of the pinned Node SDK (default maxRetries = 2) against the gateway.
// Every row counts HTTP attempts the SDK made to the gateway (via the SDK's `fetch` hook) and
// requests that actually reached the fake provider, then records them as evidence. The
// assertions encode the documented table in docs/contracts/errors-and-telemetry.md: if the
// SDK changes its policy, this test fails and the docs must be reconciled.
import assert from "node:assert/strict";
import { after, describe, it } from "node:test";
import OpenAI from "openai";
import { closedPort, directProvider, gateways, provider, secret, synthetic, writeEvidence } from "./support.ts";

interface Row {
  case: string;
  gateway_or_provider_status: string;
  sdk_outcome: string;
  sdk_attempts: number;
  provider_requests: number;
  provider_connections: number;
  elapsed_ms: number;
}

const rows: Row[] = [];
const msg = [{ role: "user" as const, content: "retry observation" }];

async function observe(
  label: string,
  gateway: string,
  params: Record<string, unknown>,
  statusLabel: string,
): Promise<Row> {
  await provider.reset();
  let attempts = 0;
  const c = new OpenAI({
    baseURL: `${gateway}/v1`,
    apiKey: synthetic.api_key,
    // maxRetries left at the SDK default on purpose: this is what an unconfigured app gets.
    fetch: async (input, init) => {
      attempts += 1;
      return fetch(input, init);
    },
  });
  const started = performance.now();
  let outcome = "ok";
  try {
    const r = await c.chat.completions.create(params as unknown as Parameters<typeof c.chat.completions.create>[0]);
    if (params.stream === true) {
      for await (const _ of r as AsyncIterable<unknown>) void _;
    }
  } catch (e) {
    outcome = e instanceof OpenAI.APIError && e.status ? `status ${e.status}` : `error ${(e as Error).constructor.name}`;
  }
  const elapsed = Math.round(performance.now() - started);
  const seen = await provider.calls();
  const row: Row = {
    case: label,
    gateway_or_provider_status: statusLabel,
    sdk_outcome: outcome,
    sdk_attempts: attempts,
    provider_requests: seen.calls.length,
    provider_connections: seen.connections,
    elapsed_ms: elapsed,
  };
  rows.push(row);
  return row;
}

describe("SDK retry observations (Node SDK, default maxRetries)", () => {
  after(() => {
    writeEvidence("retry-observations-node.json", {
      sdk: "openai (npm) 7.27.0",
      node: process.version,
      default_max_retries: 2,
      note: "attempts are HTTP requests the SDK sent to the gateway; provider_requests are those that reached the fake provider",
      rows,
    });
  });

  // Provider answers relayed unchanged by the gateway.
  for (const [status, retried] of [
    [400, false],
    [401, false],
    [403, false],
    [404, false],
    [408, true],
    [409, true],
    [422, false],
    [429, true],
    [500, true],
    [502, true],
    [503, true],
    [504, true],
  ] as const) {
    it(`provider ${status} is ${retried ? "" : "not "}retried`, async () => {
      const r = await observe(`provider ${status}`, gateways.standard, { model: `qual-err-${status}`, messages: msg }, String(status));
      assert.equal(r.sdk_attempts, retried ? 3 : 1);
      assert.equal(r.provider_requests, r.sdk_attempts, "every SDK attempt reached the provider (duplicate provider-side work)");
    });
  }

  it("provider 429 with Retry-After: 0 is retried (the relayed header is honored)", async () => {
    const r = await observe("provider 429 + Retry-After 0", gateways.standard, { model: "qual-err-429-retry-after", messages: msg }, "429");
    assert.equal(r.sdk_attempts, 3);
  });

  it("provider retry hints (x-should-retry, retry-after-ms) are dropped by the gateway", async () => {
    // Control: the SDK obeys `x-should-retry: false` when it sees it (direct to the provider) ...
    const direct = await observe(
      "provider 500 + x-should-retry false (direct, no gateway)",
      directProvider,
      { model: "qual-err-500-hint-no-retry", messages: msg },
      "500",
    );
    assert.equal(direct.sdk_attempts, 1);
    // ... but the gateway relays only allowlisted headers, so through it the hint never arrives
    // and the SDK falls back to its status-based policy (a 5xx is retried).
    const via = await observe(
      "provider 500 + x-should-retry false (through the gateway)",
      gateways.standard,
      { model: "qual-err-500-hint-no-retry", messages: msg },
      "500",
    );
    assert.equal(via.sdk_attempts, 3);
  });

  it("two provider 429s then success: the SDK retry succeeds (three provider requests, one visible result)", async () => {
    const r = await observe("provider 429, 429, then 200", gateways.standard, { model: "qual-retry-429-then-ok", messages: msg }, "429,429,200");
    assert.equal(r.sdk_outcome, "ok");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 3);
  });

  // Gateway-generated errors.
  it("gateway 422 unsupported_input is not retried and sends nothing upstream", async () => {
    const r = await observe("gateway 422 unsupported_input", gateways.standard, { model: "qual-json-ok", messages: msg, frobnicate: 1 }, "422");
    assert.equal(r.sdk_attempts, 1);
    assert.equal(r.provider_requests, 0);
  });

  it("gateway 413 limit_exceeded is not retried and sends nothing upstream", async () => {
    const r = await observe(
      "gateway 413 limit_exceeded",
      gateways.tight,
      { model: "qual-json-ok", messages: [{ role: "user", content: `${secret(51)} ${secret(52)} ${secret(53)}` }] },
      "413",
    );
    assert.equal(r.sdk_attempts, 1);
    assert.equal(r.provider_requests, 0);
  });

  it("gateway 501 not_implemented is retried (5xx) and sends nothing upstream", async () => {
    const r = await observe("gateway 501 not_implemented", gateways.noUpstream, { model: "qual-json-ok", messages: msg }, "501");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 0);
  });

  it("gateway 503 overload (Retry-After: 1) is retried after the relayed wait and never reaches the provider", async () => {
    await provider.reset();
    // Hold the only upstream and stream permit with a stalled stream, then ask for another.
    const holder = await new OpenAI({ baseURL: `${gateways.overload}/v1`, apiKey: synthetic.api_key, maxRetries: 0 }).chat.completions.create({
      model: "qual-sse-hang",
      messages: msg,
      stream: true,
    });
    const iterator = holder[Symbol.asyncIterator]();
    await iterator.next();
    const r = await observe("gateway 503 overload", gateways.overload, { model: "qual-json-ok", messages: msg }, "503");
    holder.controller.abort();
    await iterator.next().then(() => undefined, () => undefined);
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 0, "nothing was sent for the refused requests");
    assert.ok(r.elapsed_ms >= 1900, `the SDK honored Retry-After: 1 twice (took ${r.elapsed_ms} ms)`);
  });

  it("gateway 502 upstream_unavailable is retried; the provider is never reached", async () => {
    const r = await observe("gateway 502 upstream_unavailable", gateways.deadProvider, { model: "qual-json-ok", messages: msg }, "502");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 0);
  });

  it("gateway 502 upstream_response_too_large is retried and each attempt reached the provider", async () => {
    const r = await observe("gateway 502 upstream_response_too_large", gateways.standard, { model: "qual-json-oversize", messages: msg }, "502");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 3);
  });

  it("gateway 502 upstream_invalid_response is retried and each attempt reached the provider", async () => {
    const r = await observe("gateway 502 upstream_invalid_response", gateways.standard, { model: "qual-json-truncated", messages: msg }, "502");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 3);
  });

  it("gateway 504 upstream_timeout is retried and each attempt reached the provider", async () => {
    const r = await observe("gateway 504 upstream_timeout", gateways.tight, { model: "qual-hang", messages: msg }, "504");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 3);
  });

  it("a connection error to the gateway itself is retried", async () => {
    const port = await closedPort();
    const r = await observe("connection refused (gateway down)", `http://127.0.0.1:${port}`, { model: "qual-json-ok", messages: msg }, "none");
    assert.equal(r.sdk_outcome, "error APIConnectionError");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 0);
  });

  // Streams: a cut after the headers is delivered to the caller as an error and is not retried.
  it("a stream cut after the headers is not retried by the SDK", async () => {
    const r = await observe("SSE interrupted after headers", gateways.standard, { model: "qual-sse-interrupted", messages: msg, stream: true }, "200 then cut");
    assert.equal(r.sdk_attempts, 1);
    assert.equal(r.provider_requests, 1);
  });

  it("a provider 429 before the stream starts is retried like any other 429", async () => {
    const r = await observe("SSE provider 429 before headers", gateways.standard, { model: "qual-sse-err-429", messages: msg, stream: true }, "429");
    assert.equal(r.sdk_attempts, 3);
    assert.equal(r.provider_requests, 3);
  });
});
