// Supported text forms and secret handling through the pinned OpenAI Node SDK (ADR 0020).
import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";
import {
  authSha,
  client,
  gateways,
  promptMarker,
  provider,
  replaceDeep,
  secret,
  synthetic,
} from "./support.ts";

const ALLOWED_PROVIDER_HEADERS = new Set([
  "host",
  "authorization",
  "content-type",
  "content-length",
  "accept",
  "accept-encoding",
  "user-agent",
  "connection",
]);

describe("supported text forms (Node SDK, JSON)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("relays a completion and forwards roles, controls, and structure unchanged", async () => {
    const params = {
      model: "qual-json-ok",
      messages: [
        { role: "system" as const, content: "You are a synthetic test assistant." },
        { role: "developer" as const, content: "Answer briefly." },
        { role: "user" as const, content: "Say hello in Korean: 안녕 🙂" },
        { role: "assistant" as const, content: "안녕하세요" },
        { role: "user" as const, content: "Thanks." },
      ],
      temperature: 0.2,
      top_p: 0.9,
      max_completion_tokens: 64,
      presence_penalty: 0.1,
      frequency_penalty: -0.1,
      seed: 7,
      stop: ["END", "STOP"],
      user: "qual-user-1",
      response_format: { type: "json_object" as const },
      n: 1,
    };
    const res = await client(gateways.standard).chat.completions.create(params);
    assert.equal(res.choices[0]?.message.content, synthetic.stream_text);
    assert.equal(res.usage?.total_tokens, 8);
    assert.match(res._request_id ?? "", /^req_qual_/, "allowlisted provider header is relayed");

    const { connections, calls } = await provider.calls();
    assert.equal(connections, 1);
    assert.equal(calls.length, 1);
    const call = calls[0]!;
    assert.equal(call.method, "POST");
    assert.equal(call.path, "/v1/chat/completions");
    assert.deepEqual(JSON.parse(call.body), params, "no field added, dropped, or reordered semantically");
    assert.equal(call.authorization_sha256, authSha(), "caller's provider credential is forwarded");
    for (const name of call.header_names) {
      assert.ok(ALLOWED_PROVIDER_HEADERS.has(name), `unexpected header reached the provider: ${name}`);
    }
    assert.ok(!call.header_names.some((n) => n.startsWith("x-stainless")), "SDK telemetry headers are stripped");
    assert.match(call.user_agent ?? "", /^redact-secret-gateway\//, "outbound user agent is the gateway's");
  });

  it("keeps the content-parts array form as an array", async () => {
    const params = {
      model: "qual-json-ok",
      messages: [
        {
          role: "user" as const,
          content: [
            { type: "text" as const, text: "first part" },
            { type: "text" as const, text: "second part, 한국어" },
          ],
        },
      ],
    };
    const res = await client(gateways.standard).chat.completions.create(params);
    assert.equal(res.choices[0]?.message.content, synthetic.stream_text);
    const { calls } = await provider.calls();
    assert.deepEqual(JSON.parse(calls[0]!.body), params);
  });

  it("never sends a planted secret upstream, inserts a placeholder, and preserves structure", async () => {
    const marker = promptMarker();
    const params = {
      model: "qual-json-ok",
      messages: [
        { role: "system" as const, content: `Policy ${marker}. Keep answers short.` },
        { role: "user" as const, content: `my token is ${secret(1)} thanks, 안녕` },
        {
          role: "assistant" as const,
          content: [{ type: "text" as const, text: `earlier I saw ${secret(2)}` }],
        },
        { role: "user" as const, content: "and nothing secret here" },
      ],
      stop: [`end-${secret(3)}`],
      max_tokens: 32,
    };
    const res = await client(gateways.standard).chat.completions.create(params);
    assert.equal(res.choices[0]?.message.content, synthetic.stream_text);

    const { calls } = await provider.calls();
    assert.equal(calls.length, 1);
    const body = calls[0]!.body;
    for (const n of [1, 2, 3]) assert.ok(!body.includes(secret(n)), `secret ${n} reached the provider`);
    assert.ok(!body.includes("ghp_"), "secret prefix reached the provider");
    assert.equal(body.split("<SECRET_").length - 1, 3, "one placeholder per distinct planted secret");
    assert.ok(body.includes(marker), "surrounding non-secret text survives");
    assert.ok(body.includes("안녕"), "Unicode survives byte for byte");

    // Structure is identical to the request once each secret is replaced by its placeholder.
    const sent = JSON.parse(body) as typeof params;
    const expectedShape = JSON.parse(JSON.stringify(params)) as typeof params;
    assert.equal(sent.messages.length, expectedShape.messages.length);
    assert.deepEqual(
      sent.messages.map((m) => m.role),
      expectedShape.messages.map((m) => m.role),
    );
    assert.ok(Array.isArray(sent.messages[2]?.content), "parts array stays an array");
    assert.equal(sent.stop.length, 1);
    assert.equal(sent.max_tokens, 32);
    const placeholders = [...body.matchAll(/<SECRET_\d+>/g)].map((m) => m[0]);
    let restored: unknown = sent;
    for (const [i, n] of [1, 2, 3].entries()) {
      // Placeholders are numbered request-wide in traversal order (messages, then stop).
      restored = replaceDeep(restored, placeholders[i]!, secret(n));
    }
    assert.deepEqual(restored, params, "only the secrets changed");
  });
});
