// Client abort cancels the upstream exchange, and repeated cancellation leaks no capacity.
// The standard gateway has 8 upstream and 8 stream permits; 12 aborted exchanges followed by a
// normal request prove every permit came back (a leak would exhaust capacity first).
import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { client, gateways, provider, synthetic } from "./support.ts";

const msg = [{ role: "user" as const, content: "cancel me" }];

describe("cancellation (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("aborting a JSON request before the provider answers closes the upstream connection", async () => {
    const controller = new AbortController();
    const pending = client(gateways.standard).chat.completions.create(
      { model: "qual-hang", messages: msg },
      { signal: controller.signal },
    );
    assert.ok(await provider.awaitEvent(1, "received"), "provider has the request");
    controller.abort();
    await assert.rejects(pending, (e: unknown) => e instanceof OpenAI.APIUserAbortError);
    assert.ok(await provider.awaitEvent(1, "closed"), "the gateway cancelled the upstream exchange");
  });

  it("aborting an SSE stream mid-flight closes the upstream stream", async () => {
    const stream = await client(gateways.standard).chat.completions.create({
      model: "qual-sse-hang",
      messages: msg,
      stream: true,
    });
    const iterator = stream[Symbol.asyncIterator]();
    assert.equal((await iterator.next()).done, false);
    stream.controller.abort();
    await iterator.next().then(
      () => undefined,
      () => undefined,
    );
    assert.ok(await provider.awaitEvent(1, "closed"), "provider connection closed after the client went away");
  });

  it("repeated cancellation (JSON and SSE) returns every permit", async () => {
    for (let i = 1; i <= 12; i++) {
      const controller = new AbortController();
      const pending = client(gateways.standard).chat.completions.create(
        { model: "qual-hang", messages: msg },
        { signal: controller.signal },
      );
      assert.ok(await provider.awaitEvent(i, "received"));
      controller.abort();
      await pending.catch(() => undefined);
      assert.ok(await provider.awaitEvent(i, "closed"), `JSON cancel ${i} reached the provider as a close`);
    }
    const base = (await provider.calls()).calls.length;
    for (let i = 1; i <= 12; i++) {
      const stream = await client(gateways.standard).chat.completions.create({
        model: "qual-sse-hang",
        messages: msg,
        stream: true,
      });
      const iterator = stream[Symbol.asyncIterator]();
      await iterator.next();
      stream.controller.abort();
      await iterator.next().then(
        () => undefined,
        () => undefined,
      );
      assert.ok(await provider.awaitEvent(base + i, "closed"), `SSE cancel ${i} reached the provider as a close`);
    }
    const ok = await client(gateways.standard).chat.completions.create({ model: "qual-json-ok", messages: msg });
    assert.equal(ok.choices[0]?.message.content, synthetic.stream_text, "capacity fully returned");
    const sse = await client(gateways.standard).chat.completions.create({
      model: "qual-sse-ok",
      messages: msg,
      stream: true,
    });
    let n = 0;
    for await (const _ of sse) {
      void _;
      n += 1;
    }
    assert.equal(n, 8);
  });
});
