// OBSERVED retry and cancellation behavior of the pinned Node SDK against POST /v1/responses (#88).
//
// Retries: the SDK default (maxRetries 2) and the disabled setting (0) are both measured. Every row
// counts the HTTP attempts the SDK made to the gateway (via the SDK's `fetch` hook) and the
// requests and connections that actually reached the fake provider. The gateway never retries on
// its own: a provider answer is relayed once, so any extra provider delivery is an SDK retry.
// Cancellation: an abort closes the provider exchange, returns every permit, and cannot retract
// bytes that were already delivered.
import assert from "node:assert/strict";
import { after, beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { closedPort, directProvider, gateways, provider, secret, synthetic, writeEvidence } from "./support.ts";

interface Row {
  case: string;
  max_retries: string;
  sdk_outcome: string;
  sdk_attempts: number;
  provider_requests: number;
  provider_connections: number;
}
const rows: Row[] = [];

async function observe(label: string, base: string, params: Record<string, unknown>, maxRetries?: number): Promise<Row> {
  await provider.reset();
  let attempts = 0;
  const c = new OpenAI({
    baseURL: `${base}/v1`,
    apiKey: synthetic.api_key,
    ...(maxRetries === undefined ? {} : { maxRetries }),
    fetch: async (input, init) => {
      attempts += 1;
      return fetch(input, init);
    },
  });
  let outcome = "ok";
  try {
    const r = await c.responses.create(params as unknown as Parameters<typeof c.responses.create>[0]);
    if (params["stream"] === true) for await (const _ of r as AsyncIterable<unknown>) void _;
  } catch (e) {
    outcome = e instanceof OpenAI.APIError && e.status ? `status ${e.status}` : `error ${(e as Error).constructor.name}`;
  }
  const seen = await provider.calls();
  const row: Row = {
    case: label,
    max_retries: maxRetries === undefined ? "default (2)" : String(maxRetries),
    sdk_outcome: outcome,
    sdk_attempts: attempts,
    provider_requests: seen.calls.length,
    provider_connections: seen.connections,
  };
  rows.push(row);
  return row;
}

const base = { input: "retry observation", store: false };

describe("SDK retry observations, default maxRetries (Node SDK, POST /v1/responses)", () => {
  after(() => {
    writeEvidence("responses-retries-node.json", {
      sdk: "openai (npm) 7.27.0",
      node: process.version,
      note: "attempts are HTTP requests the SDK sent to the gateway; provider_requests are those that reached the fake provider",
      rows,
    });
  });

  for (const [status, retried] of [
    [400, false], [401, false], [403, false], [404, false], [408, true], [409, true], [422, false], [429, true], [500, true], [502, true], [503, true], [504, true],
  ] as const) {
    it(`provider ${status} is ${retried ? "" : "not "}retried by the SDK; the gateway adds none`, async () => {
      const r = await observe(`provider ${status}`, gateways.concurrent, { ...base, model: `qual-resp-err-${status}` });
      assert.equal(r.sdk_attempts, retried ? 3 : 1);
      assert.equal(r.provider_requests, r.sdk_attempts, "one provider delivery per SDK attempt, none added by the gateway");
    });
  }

  it("provider 429 with Retry-After: 0 is retried (the relayed header is honored)", async () => {
    assert.equal((await observe("provider 429 + Retry-After 0", gateways.concurrent, { ...base, model: "qual-resp-err-429-retry-after" })).sdk_attempts, 3);
  });

  it("provider retry hints (x-should-retry, retry-after-ms) are dropped: a 5xx is retried through the gateway", async () => {
    const direct = await observe("provider 500 + x-should-retry false (direct)", directProvider, { ...base, model: "qual-resp-err-500-hint-no-retry" });
    // Direct to the fake provider the Responses path is served too; the SDK obeys the hint.
    assert.equal(direct.sdk_attempts, 1);
    const via = await observe("provider 500 + x-should-retry false (through the gateway)", gateways.concurrent, { ...base, model: "qual-resp-err-500-hint-no-retry" });
    assert.equal(via.sdk_attempts, 3);
  });

  it("two provider 429s then success: the SDK retry succeeds, three identical sanitized deliveries", async () => {
    await provider.reset();
    let attempts = 0;
    const c = new OpenAI({ baseURL: `${gateways.concurrent}/v1`, apiKey: synthetic.api_key, fetch: async (i, n) => (attempts++, fetch(i, n)) });
    const reply = await c.responses.create({ model: "qual-resp-retry-429-then-ok", input: `retry ${secret(5)}`, store: false });
    assert.equal(reply.output_text, synthetic.stream_text);
    const { calls, connections } = await provider.calls();
    assert.equal(attempts, 3);
    assert.equal(calls.length, 3);
    assert.equal(connections, 3);
    assert.deepEqual(new Set(calls.map((x) => x.body)).size, 1, "every delivery carried the same sanitized body");
    assert.ok(calls[0]!.body.includes("<SECRET_1>") && !calls[0]!.body.includes("ghp_"));
    rows.push({ case: "provider 429, 429, then 200", max_retries: "default (2)", sdk_outcome: "ok", sdk_attempts: attempts, provider_requests: 3, provider_connections: connections });
  });

  it("gateway 422 unsupported_input (store missing) is not retried and sends nothing upstream", async () => {
    const r = await observe("gateway 422 unsupported_input", gateways.concurrent, { model: "qual-resp-json-ok", input: "x" });
    assert.deepEqual([r.sdk_attempts, r.provider_requests, r.provider_connections], [1, 0, 0]);
  });

  it("gateway 400 malformed_input (bad arguments) is not retried and sends nothing upstream", async () => {
    const r = await observe("gateway 400 malformed_input", gateways.concurrent, { ...base, model: "qual-resp-json-ok", input: [{ type: "function_call", call_id: "c", name: "n", arguments: "{" }] });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [1, 0]);
  });

  it("gateway 413 limit_exceeded is not retried and sends nothing upstream", async () => {
    const r = await observe("gateway 413 limit_exceeded", gateways.tight, { ...base, model: "qual-resp-json-ok", input: `${secret(51)} ${secret(52)} ${secret(53)}` });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [1, 0]);
  });

  it("gateway 501 not_implemented (no upstream configured) is retried (5xx) and sends nothing upstream", async () => {
    const r = await observe("gateway 501 not_implemented", gateways.noUpstream, { ...base, model: "qual-resp-json-ok" });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [3, 0]);
  });

  it("gateway 502 upstream_unavailable is retried; the provider is never reached", async () => {
    const r = await observe("gateway 502 upstream_unavailable", gateways.deadProvider, { ...base, model: "qual-resp-json-ok" });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [3, 0]);
  });

  it("gateway 504 upstream_timeout is retried and each attempt reached the provider", async () => {
    const r = await observe("gateway 504 upstream_timeout", gateways.tight, { ...base, model: "qual-resp-hang" });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [3, 3]);
  });

  it("a connection error to the gateway itself is retried", async () => {
    const r = await observe("connection refused (gateway down)", `http://127.0.0.1:${await closedPort()}`, { ...base, model: "qual-resp-json-ok" });
    assert.equal(r.sdk_outcome, "error APIConnectionError");
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [3, 0]);
  });

  it("a stream cut after the headers is not retried by the SDK", async () => {
    const r = await observe("SSE truncated after headers", gateways.concurrent, { ...base, model: "qual-resp-sse-truncated", stream: true });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [1, 1]);
  });

  it("a provider 429 before the stream starts is retried like any other 429", async () => {
    const r = await observe("SSE provider 429 before headers", gateways.concurrent, { ...base, model: "qual-resp-err-429", stream: true });
    assert.deepEqual([r.sdk_attempts, r.provider_requests], [3, 3]);
  });
});

describe("SDK retries disabled (maxRetries: 0, Node SDK, POST /v1/responses)", () => {
  for (const [label, base_, params, attempts, upstream] of [
    ["provider 429", gateways.concurrent, { ...base, model: "qual-resp-err-429" }, 1, 1],
    ["provider 500", gateways.concurrent, { ...base, model: "qual-resp-err-500" }, 1, 1],
    ["provider 429, 429, then 200 (first 429 is final)", gateways.concurrent, { ...base, model: "qual-resp-retry-429-then-ok" }, 1, 1],
    ["gateway 502 upstream_unavailable", gateways.deadProvider, { ...base, model: "qual-resp-json-ok" }, 1, 0],
    ["gateway 504 upstream_timeout", gateways.tight, { ...base, model: "qual-resp-hang" }, 1, 1],
    ["gateway 501 not_implemented", gateways.noUpstream, { ...base, model: "qual-resp-json-ok" }, 1, 0],
  ] as const) {
    it(`${label}: exactly one SDK attempt and ${upstream} provider delivery`, async () => {
      const r = await observe(label, base_, params, 0);
      assert.deepEqual([r.sdk_attempts, r.provider_requests], [attempts, upstream]);
    });
  }
});

describe("cancellation (Node SDK, POST /v1/responses)", () => {
  beforeEach(provider.reset);
  const params = { input: "cancel me", store: false } as const;
  const make = (): OpenAI => new OpenAI({ baseURL: `${gateways.standard}/v1`, apiKey: synthetic.api_key, maxRetries: 0 });

  it("aborting a JSON request closes the provider exchange; the delivered request cannot be retracted", async () => {
    const controller = new AbortController();
    const pending = make().responses.create({ ...params, input: `cancel ${secret(9)}`, model: "qual-resp-hang" }, { signal: controller.signal });
    assert.ok(await provider.awaitEvent(1, "received"), "the provider has the complete sanitized request");
    controller.abort();
    await assert.rejects(pending, (e: unknown) => e instanceof OpenAI.APIUserAbortError);
    assert.ok(await provider.awaitEvent(1, "closed"), "the gateway cancelled the provider exchange");
    const { calls } = await provider.calls();
    assert.equal(calls.length, 1, "no retry or replay after the abort");
    assert.equal((JSON.parse(calls[0]!.body) as { input: string }).input, "cancel <SECRET_1>", "bytes already delivered stay delivered");
  });

  it("aborting a stream after its first event closes the provider exchange and replays nothing", async () => {
    const stream = await make().responses.create({ ...params, model: "qual-resp-sse-hang", stream: true });
    const iterator = stream[Symbol.asyncIterator]();
    assert.equal((await iterator.next()).done, false);
    stream.controller.abort();
    await iterator.next().then(() => undefined, () => undefined);
    assert.ok(await provider.awaitEvent(1, "closed"));
    assert.equal((await provider.calls()).calls.length, 1);
  });

  it("the responses.stream helper: aborting it closes the provider exchange", async () => {
    const helper = make().responses.stream({ ...params, model: "qual-resp-sse-hang" });
    const seen: string[] = [];
    helper.on("event", (e) => seen.push(e.type));
    const started = new Promise<void>((resolve) => helper.on("event", () => resolve()));
    await started;
    helper.abort();
    await helper.done().then(() => undefined, () => undefined);
    assert.ok(await provider.awaitEvent(1, "closed"));
    assert.ok(seen.length >= 1 && !seen.includes("response.completed"));
  });

  it("repeated cancellation (12 JSON, 12 SSE) returns every permit", async () => {
    for (let i = 1; i <= 12; i++) {
      const controller = new AbortController();
      const pending = make().responses.create({ ...params, model: "qual-resp-hang" }, { signal: controller.signal });
      assert.ok(await provider.awaitEvent(i, "received"));
      controller.abort();
      await pending.catch(() => undefined);
      assert.ok(await provider.awaitEvent(i, "closed"), `JSON cancel ${i} reached the provider as a close`);
    }
    const first = (await provider.calls()).calls.length;
    for (let i = 1; i <= 12; i++) {
      const stream = await make().responses.create({ ...params, model: "qual-resp-sse-hang", stream: true });
      const iterator = stream[Symbol.asyncIterator]();
      await iterator.next();
      stream.controller.abort();
      await iterator.next().then(() => undefined, () => undefined);
      assert.ok(await provider.awaitEvent(first + i, "closed"), `SSE cancel ${i} reached the provider as a close`);
    }
    const ok = await make().responses.create({ ...params, model: "qual-resp-json-ok" });
    assert.equal(ok.output_text, synthetic.stream_text, "capacity fully returned");
    const sse = await make().responses.create({ ...params, model: "qual-resp-sse-ok", stream: true });
    let terminal = false;
    for await (const e of sse) terminal ||= e.type === "response.completed";
    assert.ok(terminal);
  });

  it("a caller abort on a Chat request does not disturb a Responses request in flight on the same gateway", async () => {
    const slow = make().responses.create({ ...params, model: "qual-resp-json-slow" });
    const controller = new AbortController();
    const chat = make().chat.completions.create({ model: "qual-hang", messages: [{ role: "user", content: "x" }] }, { signal: controller.signal });
    assert.ok(await provider.awaitCalls(2));
    controller.abort();
    await chat.catch(() => undefined);
    assert.equal((await slow).output_text, synthetic.stream_text);
  });
});
