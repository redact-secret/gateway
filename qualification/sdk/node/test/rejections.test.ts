// Rejected inputs never deliver upstream body bytes, seen through the pinned Node SDK.
// "Never" is asserted on the fake provider's own record: zero TCP connections and zero
// requests (ADR 0020).
import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { assertNothingUpstream, client, gateways, leaks, promptMarker, provider, secret } from "./support.ts";

type Params = Parameters<OpenAI["chat"]["completions"]["create"]>[0];

const base = {
  model: "qual-json-ok",
  messages: [{ role: "user" as const, content: "hello" }],
};

async function expectRejected(
  gateway: string,
  params: Record<string, unknown>,
  status: number,
  code: string,
  why: string,
): Promise<void> {
  await provider.reset();
  const c = client(gateway);
  let caught: unknown;
  try {
    await c.chat.completions.create(params as unknown as Params);
  } catch (e) {
    caught = e;
  }
  assert.ok(caught instanceof OpenAI.APIError, `${why}: expected an API error`);
  assert.equal(caught.status, status, `${why}: status`);
  assert.equal(caught.code, code, `${why}: safe error code`);
  const rendered = JSON.stringify({ m: caught.message, e: caught.error, h: [...(caught.headers?.entries?.() ?? [])] });
  assert.deepEqual(leaks(rendered), [], `${why}: error leaks request text`);
  await assertNothingUpstream(assert, why);
}

describe("unsupported or unknown inputs are rejected locally (422 unsupported_input)", () => {
  const marker = promptMarker();
  const planted = `secret ${secret(10)} ${marker}`;
  const cases: Array<[string, Record<string, unknown>]> = [
    ["unknown top-level field", { ...base, frobnicate: 1 }],
    ["tools", { ...base, tools: [{ type: "function", function: { name: "f", parameters: {} } }] }],
    ["tool_choice", { ...base, tool_choice: "auto" }],
    [
      "image_url content part",
      {
        model: base.model,
        messages: [{ role: "user", content: [{ type: "image_url", image_url: { url: "https://example.invalid/x.png" } }] }],
      },
    ],
    [
      "unknown field inside a text part",
      { model: base.model, messages: [{ role: "user", content: [{ type: "text", text: "hi", cache_control: 1 }] }] },
    ],
    ["metadata", { ...base, metadata: { k: "v" } }],
    ["logprobs", { ...base, logprobs: true }],
    ["participant name on a message", { model: base.model, messages: [{ role: "user", name: "alice", content: "hi" }] }],
    ["tool role message", { model: base.model, messages: [{ role: "tool", tool_call_id: "x", content: "hi" }] }],
    ["null content", { model: base.model, messages: [{ role: "assistant", content: null }] }],
    ["n greater than one", { ...base, n: 2 }],
    [
      "response_format json_schema",
      { ...base, response_format: { type: "json_schema", json_schema: { name: "s", schema: {} } } },
    ],
    ["secret hidden next to an unknown field", { model: base.model, messages: [{ role: "user", content: planted }], frobnicate: 1 }],
    ["stream request with an unknown field", { ...base, stream: true, frobnicate: 1 }],
  ];
  for (const [name, params] of cases) {
    it(name, async () => {
      await expectRejected(gateways.standard, params, 422, "unsupported_input", name);
    });
  }
});

describe("core completeness and policy failures never forward", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("a finding in model is rejected, not rewritten", async () => {
    await expectRejected(
      gateways.standard,
      { model: secret(11), messages: [{ role: "user", content: "hi" }] },
      422,
      "unsupported_input",
      "secret in model",
    );
  });

  it("a Warn finding is rejected under the default policy", async () => {
    await expectRejected(
      gateways.standard,
      { ...base, messages: [{ role: "user", content: "password=hunter2xyz" }] },
      422,
      "unsupported_input",
      "warn finding",
    );
  });

  it("finding-limit exhaustion rejects the whole request (limit_exceeded), nothing partial", async () => {
    const content = [secret(21), secret(22), secret(23)].join(" and ");
    await expectRejected(
      gateways.tight,
      { ...base, messages: [{ role: "user", content }] },
      413,
      "limit_exceeded",
      "finding limit",
    );
  });
});

describe("over-limit requests never forward (413 limit_exceeded)", () => {
  it("body over max_body_bytes", async () => {
    await expectRejected(
      gateways.tight,
      { ...base, messages: [{ role: "user", content: "x".repeat(8192) }] },
      413,
      "limit_exceeded",
      "oversize body",
    );
  });

  it("more messages than max_messages", async () => {
    const messages = Array.from({ length: 5 }, () => ({ role: "user" as const, content: "hi" }));
    await expectRejected(gateways.tight, { ...base, messages }, 413, "limit_exceeded", "too many messages");
  });
});

describe("credential and deployment rejections never forward", () => {
  it("a request with no provider credential is 401 missing_credential", async () => {
    await provider.reset();
    const c = client(gateways.standard, { defaultHeaders: { Authorization: null } });
    let caught: unknown;
    try {
      await c.chat.completions.create(base);
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.APIError);
    assert.equal(caught.status, 401);
    assert.equal(caught.code, "missing_credential");
    await assertNothingUpstream(assert, "missing credential");
  });

  it("a deployment with no upstream answers 501 not_implemented", async () => {
    await provider.reset();
    const planted = { ...base, messages: [{ role: "user" as const, content: `x ${secret(31)}` }] };
    for (const stream of [false, true]) {
      let caught: unknown;
      try {
        await client(gateways.noUpstream).chat.completions.create({ ...planted, stream } as Params);
      } catch (e) {
        caught = e;
      }
      assert.ok(caught instanceof OpenAI.APIError);
      assert.equal(caught.status, 501);
      assert.equal(caught.code, "not_implemented");
    }
    await assertNothingUpstream(assert, "no upstream configured");
  });
});
