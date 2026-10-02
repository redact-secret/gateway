// SSE streaming through the pinned Node SDK: fragmented, multibyte, multi-event, slow, gated
// (incremental), interrupted, and provider-error streams.
import assert from "node:assert/strict";
import { after, beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { client, directProvider, gateways, promptMarker, provider, secret, synthetic, writeEvidence } from "./support.ts";

const msg = [{ role: "user" as const, content: "stream please" }];

async function collect(model: string, gateway = gateways.standard) {
  const stream = await client(gateway).chat.completions.create({ model, messages: msg, stream: true });
  let text = "";
  let chunks = 0;
  let finish: string | null = null;
  for await (const chunk of stream) {
    chunks += 1;
    text += chunk.choices[0]?.delta?.content ?? "";
    finish = chunk.choices[0]?.finish_reason ?? finish;
  }
  return { text, chunks, finish };
}

// Observed, not assumed: whether the SDK RAISES on a truncated stream. The gateway ends such a
// stream without the terminating chunk (ADR 0018) but, since ADR 0019, also sends
// `Connection: close`; Node's fetch (undici) then reports a clean end for the cut, so the SDK
// yields a shorter stream and no error. The safe invariant asserted above is that no completion
// is ever fabricated; applications must require a `finish_reason` themselves.
const observations: Array<Record<string, unknown>> = [];
after(() => {
  writeEvidence("stream-truncation-observations-node.json", {
    sdk: "openai (npm) 7.27.0",
    node: process.version,
    observations,
  });
});

async function drain(model: string, gateway: string) {
  const stream = await client(gateway).chat.completions.create({ model, messages: msg, stream: true });
  let text = "";
  let events = 0;
  let finish: string | null = null;
  let raised = false;
  try {
    for await (const chunk of stream) {
      events += 1;
      text += chunk.choices[0]?.delta?.content ?? "";
      finish = chunk.choices[0]?.finish_reason ?? finish;
    }
  } catch {
    raised = true;
  }
  return { text, events, finish, raised };
}

describe("SSE streaming (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  for (const model of ["qual-sse-ok", "qual-sse-multi", "qual-sse-fragmented"]) {
    it(`${model}: events arrive intact and in order, multibyte text included`, async () => {
      const r = await collect(model);
      assert.equal(r.text, synthetic.stream_text);
      assert.equal(r.chunks, 8, "role event, six text events, final event");
      assert.equal(r.finish, "stop");
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      assert.equal(JSON.parse(calls[0]!.body).stream, true, "stream flag forwarded");
    });
  }

  it("a streamed request is sanitized exactly like any other (planted secret never upstream)", async () => {
    const marker = promptMarker();
    const stream = await client(gateways.standard).chat.completions.create({
      model: "qual-sse-ok",
      stream: true,
      stream_options: { include_usage: true },
      messages: [{ role: "user", content: `${marker} token ${secret(41)}` }],
    });
    for await (const _ of stream) void _;
    const body = (await provider.calls()).calls[0]!.body;
    assert.ok(!body.includes(secret(41)) && !body.includes("ghp_"));
    assert.ok(body.includes("<SECRET_1>") && body.includes(marker));
    assert.deepEqual(JSON.parse(body).stream_options, { include_usage: true });
  });

  it("relays incrementally: an early event reaches the SDK while the provider still holds the stream open", async () => {
    const stream = await client(gateways.standard).chat.completions.create({
      model: "qual-sse-gated",
      messages: msg,
      stream: true,
    });
    const iterator = stream[Symbol.asyncIterator]();
    const first = await iterator.next();
    assert.equal(first.done, false);
    assert.ok(await provider.awaitEvent(1, "gated"), "provider is holding the rest of the stream");
    const held = (await provider.calls()).calls[0]!;
    assert.ok(!held.events.includes("finished"), "provider has not finished: the gateway did not buffer the stream");
    await provider.release(1);
    let text = "";
    let seen = 1;
    for (let n = await iterator.next(); !n.done; n = await iterator.next()) {
      seen += 1;
      text += n.value.choices[0]?.delta?.content ?? "";
    }
    assert.equal(seen, 8);
    assert.equal(text, synthetic.stream_text);
    assert.ok(await provider.awaitEvent(1, "finished"));
  });

  it("a slow provider stream arrives spread over time, not as one block", async () => {
    const stream = await client(gateways.standard).chat.completions.create({
      model: "qual-sse-slow",
      messages: msg,
      stream: true,
    });
    const arrivals: number[] = [];
    for await (const _ of stream) {
      void _;
      arrivals.push(performance.now());
    }
    assert.equal(arrivals.length, 8);
    assert.ok(arrivals.at(-1)! - arrivals[0]! >= 100, "provider paces events 25 ms apart; the relay must not batch them");
  });

  it("an interrupted provider stream never yields a fabricated completion (see the Node/undici note)", async () => {
    const r = await drain("qual-sse-interrupted", gateways.standard);
    assert.equal(r.finish, null, "no completion (finish_reason) was fabricated");
    assert.ok(r.events <= 2, "only the events the provider really sent arrived");
    assert.ok(r.text.length < synthetic.stream_text.length, "the text is visibly incomplete");
    observations.push({ case: "provider cut the stream after two events", sdk_raised: r.raised, events: r.events, finish_reason: r.finish });
  });

  it("control: undici raises on a truncated chunked stream on a keep-alive connection but not on `Connection: close`", async () => {
    async function direct(model: string): Promise<string> {
      const r = await fetch(`${directProvider}/v1/chat/completions`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model, stream: true, messages: msg }),
      });
      try {
        await r.text();
        return "clean end";
      } catch {
        return "error";
      }
    }
    const keepAlive = await direct("qual-sse-interrupted");
    const close = await direct("qual-sse-interrupted-close");
    observations.push({ case: "control: direct truncated chunked stream", keep_alive: keepAlive, connection_close: close });
    assert.equal(keepAlive, "error", "truncation on a keep-alive connection is detected");
    assert.equal(close, "clean end", "truncation on a `Connection: close` response is not (undici behavior)");
  });

  it("a stalled provider stream is cut by the gateway's idle deadline, again with no fabricated completion", async () => {
    const r = await drain("qual-sse-hang", gateways.tight);
    assert.equal(r.finish, null);
    assert.ok(r.events <= 2);
    assert.ok(await provider.awaitEvent(1, "closed"), "gateway closed the provider connection");
    observations.push({ case: "gateway idle deadline cut the stream", sdk_raised: r.raised, events: r.events, finish_reason: r.finish });
  });

  it("a provider error before the stream starts is a normal SDK API error", async () => {
    let caught: unknown;
    try {
      await client(gateways.standard).chat.completions.create({ model: "qual-sse-err-429", messages: msg, stream: true });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.RateLimitError);
    assert.equal(caught.status, 429);
  });
});
