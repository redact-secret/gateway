// The SDK's `responses.stream()` helper (accumulating stream with a final response) against the
// Responses relay (#88). Terminal events come from the provider, never from the gateway: a normal
// completion yields the final response; a provider-declared failed/incomplete outcome is content;
// a clean end without a terminal event and an abrupt cut never yield a completed response.
// What the helper RAISES on a cut depends on the runtime, so it is recorded and not asserted.
import assert from "node:assert/strict";
import { after, beforeEach, describe, it } from "node:test";
import { client, gateways, provider, secret, synthetic, writeEvidence } from "./support.ts";

const observations: Array<Record<string, unknown>> = [];
after(() => {
  writeEvidence("responses-stream-helper-node.json", { sdk: "openai (npm) 7.27.0", node: process.version, observations });
});

async function run(model: string, gateway = gateways.standard): Promise<{ types: string[]; final: { status?: string; output_text?: string } | null; raised: string | null }> {
  const helper = client(gateway).responses.stream({ model, input: `helper ${secret(3)}`, store: false });
  const types: string[] = [];
  let raised: string | null = null;
  let final: { status?: string; output_text?: string } | null = null;
  try {
    for await (const e of helper) types.push(e.type);
    final = await helper.finalResponse();
  } catch (e) {
    raised = (e as Error).constructor.name;
  }
  return { types, final, raised };
}

describe("responses.stream() helper (Node SDK)", () => {
  beforeEach(provider.reset);

  it("a completed stream: final response with the relayed text; the request was sanitized and sent once with stream:true", async () => {
    const r = await run("qual-resp-sse-ok");
    assert.equal(r.raised, null);
    assert.equal(r.final?.status, "completed");
    assert.equal(r.final?.output_text, synthetic.stream_text);
    assert.equal(r.types[0], "response.created");
    assert.equal(r.types.at(-1), "response.completed");
    const { calls, connections } = await provider.calls();
    assert.deepEqual([calls.length, connections], [1, 1]);
    const body = JSON.parse(calls[0]!.body) as { stream: boolean; store: boolean; input: string };
    assert.deepEqual([body.stream, body.store, body.input], [true, false, "helper <SECRET_1>"]);
    observations.push({ case: "completed", raised: r.raised, final_status: r.final?.status });
  });

  for (const [model, status] of [["qual-resp-sse-failed", "failed"], ["qual-resp-sse-incomplete", "incomplete"]] as const) {
    it(`${model}: the provider's ${status} outcome is the final response, not an error`, async () => {
      const r = await run(model);
      assert.equal(r.types.at(-1), `response.${status}`);
      assert.ok(!r.types.includes("response.completed"));
      observations.push({ case: model, raised: r.raised, final_status: r.final?.status ?? null });
    });
  }

  for (const [model, gateway] of [
    ["qual-resp-sse-clean-no-terminal", gateways.standard],
    ["qual-resp-sse-truncated", gateways.standard],
    ["qual-resp-sse-hang", gateways.tight],
  ] as const) {
    it(`${model}: never a completed final response`, async () => {
      const r = await run(model, gateway);
      assert.ok(!r.types.includes("response.completed"));
      assert.notEqual(r.final?.status, "completed");
      assert.equal((await provider.calls()).calls.length, 1, "no retry or resume");
      observations.push({ case: model, raised: r.raised, final_status: r.final?.status ?? null, events: r.types.length });
    });
  }
});
