// JSON responses, provider errors, and gateway transport errors through the pinned Node SDK.
import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { client, gateways, provider, synthetic } from "./support.ts";

const msg = [{ role: "user" as const, content: "hello" }];

async function failure(gateway: string, model: string): Promise<InstanceType<typeof OpenAI.APIError>> {
  try {
    await client(gateway).chat.completions.create({ model, messages: msg });
  } catch (e) {
    assert.ok(e instanceof OpenAI.APIError, `expected APIError, got ${String(e)}`);
    return e;
  }
  assert.fail("expected the request to fail");
}

describe("JSON responses (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("relays a large completion body intact", async () => {
    const res = await client(gateways.standard).chat.completions.create({ model: "qual-json-large", messages: msg });
    assert.equal(res.choices[0]?.message.content?.length, "synthetic large reply ".length * 3000);
  });

  it("relays provider 4xx/5xx statuses and bodies unchanged (they are provider answers, not gateway errors)", async () => {
    const expected: Array<[number, Function]> = [
      [400, OpenAI.BadRequestError],
      [401, OpenAI.AuthenticationError],
      [403, OpenAI.PermissionDeniedError],
      [404, OpenAI.NotFoundError],
      [409, OpenAI.ConflictError],
      [422, OpenAI.UnprocessableEntityError],
      [429, OpenAI.RateLimitError],
      [500, OpenAI.InternalServerError],
      [503, OpenAI.InternalServerError],
    ];
    for (const [status, cls] of expected) {
      const e = await failure(gateways.standard, `qual-err-${status}`);
      assert.equal(e.status, status);
      assert.ok(e instanceof cls, `status ${status} maps to ${cls.name}`);
      assert.deepEqual(
        e.error,
        { message: `synthetic provider error ${status}`, type: "synthetic_error", param: null, code: `synthetic_${status}` },
        "provider JSON error body arrives unchanged (not the gateway envelope)",
      );
    }
  });

  it("answers an oversize provider response with a safe gateway 502, never a partial body", async () => {
    const e = await failure(gateways.standard, "qual-json-oversize");
    assert.equal(e.status, 502);
    assert.equal(e.code, "upstream_response_too_large");
  });

  it("answers a truncated provider response with 502 upstream_invalid_response", async () => {
    const e = await failure(gateways.standard, "qual-json-truncated");
    assert.equal(e.status, 502);
    assert.equal(e.code, "upstream_invalid_response");
  });

  it("answers an unreachable provider with 502 upstream_unavailable", async () => {
    const e = await failure(gateways.deadProvider, "qual-json-ok");
    assert.equal(e.status, 502);
    assert.equal(e.code, "upstream_unavailable");
  });

  it("answers a provider that never replies with 504 upstream_timeout (header deadline)", async () => {
    const e = await failure(gateways.tight, "qual-hang");
    assert.equal(e.status, 504);
    assert.equal(e.code, "upstream_timeout");
  });

  it("gateway-generated errors carry the fixed envelope only", async () => {
    const e = await failure(gateways.noUpstream, "qual-json-ok");
    assert.deepEqual(e.error, { code: "not_implemented" }, "the SDK exposes the gateway envelope's inner error object");
    assert.ok(!JSON.stringify(e.error).includes(synthetic.api_key));
  });
});
