// Responses (`POST /v1/responses`) JSON and SSE relay lifecycle through the pinned Node SDK (#87).
//
// The terminal signal of a Responses stream is the provider's own event: `response.completed`,
// `response.failed` or `response.incomplete`. It is NOT Chat's `finish_reason` and not `[DONE]`.
// Three different things can end an iteration, and a caller has to tell them apart:
//   1. a terminal event the provider sent (completed, or a provider-declared failed/incomplete);
//   2. a clean transport end with no terminal event (a provider or intermediary that closed early);
//   3. an abrupt transport cut (the gateway or the provider dropped the connection).
// The gateway adds no event, status or retry in any case. Whether the SDK RAISES on case 3 depends
// on the Node.js version (see streaming.test.ts), so only the terminal event is asserted; whether it
// raised is recorded as evidence.
import assert from "node:assert/strict";
import { after, beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { client, gateways, promptMarker, provider, secret, synthetic, writeEvidence } from "./support.ts";

type Terminal = "completed" | "failed" | "incomplete" | null;

interface Outcome {
  terminal: Terminal;
  types: string[];
  text: string;
  args: string;
  raised: boolean;
}

const TERMINALS: Record<string, Terminal> = {
  "response.completed": "completed",
  "response.failed": "failed",
  "response.incomplete": "incomplete",
};

async function collect(model: string, gateway = gateways.standard, signal?: AbortSignal): Promise<Outcome> {
  const out: Outcome = { terminal: null, types: [], text: "", args: "", raised: false };
  try {
    const stream = await client(gateway).responses.create(
      { model, input: "stream please", store: false, stream: true },
      signal ? { signal } : undefined,
    );
    for await (const event of stream) {
      out.types.push(event.type);
      if (event.type === "response.output_text.delta") out.text += event.delta;
      if (event.type === "response.function_call_arguments.delta") out.args += event.delta;
      out.terminal = TERMINALS[event.type] ?? out.terminal;
    }
  } catch {
    out.raised = true;
  }
  return out;
}

const observations: Array<Record<string, unknown>> = [];
after(() => {
  writeEvidence("responses-lifecycle-observations-node.json", {
    sdk: "openai (npm) 7.27.0",
    node: process.version,
    observations,
  });
});

describe("Responses relay lifecycle (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("a completed stream: events intact, terminal response.completed, request sanitized and sent once to /v1/responses", async () => {
    const marker = promptMarker();
    const stream = await client(gateways.standard).responses.create({
      model: "qual-resp-sse-ok",
      input: `${marker} token ${secret(87)}`,
      store: false,
      stream: true,
    });
    const types: string[] = [];
    let text = "";
    let terminal: Terminal = null;
    for await (const event of stream) {
      types.push(event.type);
      if (event.type === "response.output_text.delta") text += event.delta;
      terminal = TERMINALS[event.type] ?? terminal;
    }
    assert.equal(text, synthetic.stream_text);
    assert.equal(terminal, "completed");
    assert.equal(types[0], "response.created");
    assert.equal(types.at(-1), "response.completed");
    const { calls } = await provider.calls();
    assert.equal(calls.length, 1);
    assert.equal(calls[0]!.path, "/v1/responses");
    const body = calls[0]!.body;
    assert.ok(!body.includes(secret(87)) && !body.includes("ghp_"));
    assert.ok(body.includes("<SECRET_1>") && body.includes(marker));
    assert.equal(JSON.parse(body).stream, true);
    assert.equal(JSON.parse(body).store, false);
  });

  it("function-call argument deltas arrive in order and the stream completes", async () => {
    const r = await collect("qual-resp-sse-tool");
    assert.equal(r.raised, false);
    assert.equal(r.args, '{"city":"서울"}');
    assert.deepEqual(JSON.parse(r.args), { city: "서울" });
    assert.equal(r.terminal, "completed");
  });

  for (const [model, expected] of [
    ["qual-resp-sse-failed", "failed"],
    ["qual-resp-sse-incomplete", "incomplete"],
  ] as const) {
    it(`${model}: a provider-declared ${expected} outcome is relayed as the provider's own terminal event`, async () => {
      const r = await collect(model);
      assert.equal(r.terminal, expected, "the SDK sees the provider's terminal event, not a gateway one");
      assert.equal(r.types.at(-1), `response.${expected}`);
      assert.ok(!r.types.includes("response.completed"), "no completion was fabricated");
      assert.equal(r.raised, false, "a provider-declared outcome is content, the transport ended normally");
      assert.equal((await provider.calls()).calls.length, 1, "the gateway never retries");
      observations.push({ case: model, terminal: r.terminal, sdk_raised: r.raised });
    });
  }

  for (const [model, gateway, why] of [
    ["qual-resp-sse-clean-no-terminal", gateways.standard, "provider ended the stream cleanly without a terminal event"],
    ["qual-resp-sse-truncated", gateways.standard, "provider dropped the connection mid-stream"],
    ["qual-resp-sse-hang", gateways.tight, "provider went silent and the gateway's idle deadline cut the stream"],
  ] as const) {
    it(`${model}: no terminal event is ever fabricated (${why})`, async () => {
      const r = await collect(model, gateway);
      assert.equal(r.terminal, null, "no completed/failed/incomplete event: the response is NOT complete");
      assert.ok(!r.types.includes("response.completed"));
      assert.ok(!r.types.includes("response.failed") && !r.types.includes("error"));
      assert.ok(r.types.length <= 2, "only the events the provider really sent arrived");
      assert.equal((await provider.calls()).calls.length, 1, "the gateway never retries or resumes");
      observations.push({ case: model, why, terminal: r.terminal, events: r.types.length, sdk_raised: r.raised });
    });
  }

  it("a client abort after the headers closes the provider exchange and nothing is replayed", async () => {
    const controller = new AbortController();
    const stream = await client(gateways.standard).responses.create(
      { model: "qual-resp-sse-hang", input: "abort me", store: false, stream: true },
      { signal: controller.signal },
    );
    const iterator = stream[Symbol.asyncIterator]();
    const first = await iterator.next();
    assert.equal(first.done, false);
    controller.abort();
    // Whether the aborted iteration raises or just ends is the SDK's business; both are an abort.
    try {
      for (let n = await iterator.next(); !n.done; n = await iterator.next()) void n;
    } catch {
      /* aborted */
    }
    assert.ok(await provider.awaitEvent(1, "closed"), "the gateway dropped the provider connection");
    assert.equal((await provider.calls()).calls.length, 1);
  });

  it("ordinary JSON: completed, and provider-declared failed/incomplete are 200 bodies the SDK returns unchanged", async () => {
    const c = client(gateways.standard).responses;
    const ok = await c.create({ model: "qual-resp-json-ok", input: "hi", store: false });
    assert.equal(ok.status, "completed");
    assert.equal(ok.output_text, synthetic.stream_text);
    const failed = await c.create({ model: "qual-resp-json-failed", input: "hi", store: false });
    assert.equal(failed.status, "failed");
    assert.equal(failed.error?.code, "server_error");
    const incomplete = await c.create({ model: "qual-resp-json-incomplete", input: "hi", store: false });
    assert.equal(incomplete.status, "incomplete");
    assert.equal(incomplete.incomplete_details?.reason, "max_output_tokens");
    assert.equal((await provider.calls()).calls.length, 3);
  });

  it("a provider error status is a normal SDK API error with the provider's own body", async () => {
    let caught: unknown;
    try {
      await client(gateways.standard).responses.create({ model: "qual-resp-err-429", input: "hi", store: false });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.RateLimitError);
    assert.equal(caught.status, 429);
    assert.equal((await provider.calls()).calls.length, 1);
  });

  it("a rejected Responses request (store missing) never reaches the provider", async () => {
    let caught: unknown;
    try {
      await client(gateways.standard).responses.create({ model: "qual-resp-json-ok", input: "hi" });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.APIError);
    assert.ok(caught.status !== undefined && caught.status >= 400 && caught.status < 500);
    const { connections, calls } = await provider.calls();
    assert.equal(calls.length, 0);
    assert.equal(connections, 0);
  });
});
