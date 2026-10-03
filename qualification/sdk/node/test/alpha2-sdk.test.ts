// Alpha 2 behavior that needs the SDK itself (#57): the SDK's real zod helpers, a tool round trip
// driven by typed SDK objects, the verbatim-replay incompatibility, and request-state isolation
// under concurrency. The row-by-row field and policy matrix is alpha2-cases.test.ts.
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { describe, it, beforeEach, after } from "node:test";
import OpenAI from "openai";
import { zodFunction, zodResponseFormat } from "openai/helpers/zod";
import { z } from "zod";
import { assertNothingUpstream, client, gateways, leaks, provider, secret, synthetic, writeEvidence } from "./support.ts";
import { normalize, plantedIn, type Json } from "./alpha2-support.ts";

type Params = Parameters<OpenAI["chat"]["completions"]["create"]>[0];
type Completion = OpenAI.Chat.Completions.ChatCompletion;

const observations: Record<string, unknown> = {};
after(() => writeEvidence("alpha2-sdk-node.json", { sdk: "openai (npm) 7.27.0", zod: "4.6.5", node: process.version, observations }));

const weather = (n: number) => z.object({
  city: z.string().describe(`City name ${secret(n)}`),
  days: z.number().int().min(1).max(7),
  unit: z.enum(["c", "f"]),
  note: z.string().nullable(),
  tags: z.array(z.string()),
});
const Weather = weather(1);

/** What an application must do to the zod helper's output: remove the draft-07 `$schema` key. */
function withoutDollarSchema<T>(holder: T): T {
  const h = holder as { parameters?: unknown; schema?: unknown };
  delete (((h.parameters ?? h.schema) as Record<string, unknown>))["$schema"];
  return holder;
}

async function sdkError(run: () => Promise<unknown>): Promise<InstanceType<typeof OpenAI.APIError>> {
  try {
    await run();
  } catch (e) {
    assert.ok(e instanceof OpenAI.APIError, String(e));
    return e;
  }
  assert.fail("expected the request to be rejected");
}

describe("zod helpers (zodResponseFormat / zodFunction, openai/helpers/zod) against the gateway", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("the helper output as emitted is rejected (422): zod-to-json-schema writes $schema; nothing goes upstream", async () => {
    const emittedFormat = zodResponseFormat(Weather, "weather");
    const emittedTool = zodFunction({ name: "get_weather", parameters: Weather });
    assert.ok("$schema" in (emittedFormat.json_schema.schema as object), "the helper really emits $schema");
    for (const [what, extra] of [
      ["zodResponseFormat", { response_format: emittedFormat }],
      ["zodFunction", { tools: [emittedTool] }],
    ] as const) {
      const e = await sdkError(() =>
        client(gateways.standard).chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: "q" }], ...extra } as Params),
      );
      assert.equal(e.status, 422, what);
      assert.equal(e.code, "unsupported_input", what);
      assert.deepEqual(leaks(JSON.stringify([e.message, e.error])), [], what);
      await assertNothingUpstream(assert, what);
    }
    observations["zod_as_emitted"] = "rejected 422 unsupported_input ($schema)";
  });

  it("with $schema removed, zodResponseFormat and zodFunction output is forwarded with the description redacted and every other structure retained", async () => {
    const format = withoutDollarSchema(zodResponseFormat(weather(2), "weather").json_schema);
    const tool = zodFunction({ name: "get_weather", parameters: Weather, description: "Look up weather." });
    withoutDollarSchema(tool.function as { parameters?: unknown });
    const params = {
      model: "qual-json-ok",
      messages: [{ role: "user" as const, content: "weather in 서울?" }],
      tools: [tool],
      tool_choice: "auto" as const,
      parallel_tool_calls: false,
      response_format: { type: "json_schema" as const, json_schema: format },
    };
    const res = await client(gateways.standard).chat.completions.create(params as Params);
    assert.equal((res as Completion).choices[0]?.message.content, synthetic.stream_text);
    const { calls } = await provider.calls();
    assert.equal(calls.length, 1);
    const sent = JSON.parse(calls[0]!.body) as Record<string, unknown>;
    const expected = JSON.parse(JSON.stringify(params)) as Json;
    // Traversal order: tools before response_format, so the tool description is 1, the format's is 2.
    const text = JSON.stringify(expected).replace(secret(1), "<SECRET_1>").replace(secret(2), "<SECRET_2>");
    assert.deepEqual(sent, JSON.parse(text));
    assert.ok(!calls[0]!.body.includes("ghp_"));
    const fn = (sent["tools"] as Array<{ function: { strict: boolean; parameters: { required: string[]; additionalProperties: boolean } } }>)[0]!.function;
    assert.equal(fn.strict, true, "strict retained");
    assert.deepEqual(fn.parameters.required, ["city", "days", "unit", "note", "tags"]);
    assert.equal(fn.parameters.additionalProperties, false);
    observations["zod_without_dollar_schema"] = "forwarded, description redacted in place, strict/required/additionalProperties retained";
  });

  it("zod features that emit format, pattern or default stay rejected even after $schema is removed", async () => {
    const shapes: Array<[string, z.ZodType]> = [
      ["email (format + pattern)", z.object({ email: z.string().email() })],
      ["regex (pattern)", z.object({ code: z.string().regex(/^[A-Z]{3}$/) })],
      ["default value", z.object({ unit: z.string().default("c") })],
    ];
    for (const [name, shape] of shapes) {
      const format = withoutDollarSchema(zodResponseFormat(shape, "s").json_schema);
      const e = await sdkError(() =>
        client(gateways.standard).chat.completions.create({
          model: "qual-json-ok",
          messages: [{ role: "user", content: "q" }],
          response_format: { type: "json_schema", json_schema: format },
        } as Params),
      );
      assert.equal(e.status, 422, name);
      assert.equal(e.code, "unsupported_input", name);
      await assertNothingUpstream(assert, name);
    }
    observations["zod_unsupported_features"] = shapes.map(([n]) => n);
  });
});

describe("tool round trips with typed SDK objects (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  const tool = zodFunction({ name: "get_weather", parameters: z.object({ city: z.string(), days: z.number().int() }) });
  withoutDollarSchema(tool.function as { parameters?: unknown });

  function historyFrom(message: Completion["choices"][number]["message"]) {
    // Only the documented fields are replayed: role, content (null), and tool_calls as id/type/function.
    const toolCalls = (message.tool_calls ?? []).map((tc) => {
      assert.equal(tc.type, "function");
      return { id: tc.id, type: "function" as const, function: { name: (tc as { function: { name: string } }).function.name, arguments: (tc as { function: { arguments: string } }).function.arguments } };
    });
    return { role: "assistant" as const, content: null, tool_calls: toolCalls };
  }

  for (const [label, model, results] of [
    ["one call", "qual-json-tool-call", 1],
    ["parallel calls", "qual-json-tool-calls-parallel", 2],
  ] as const) {
    it(`${label}: the app receives tool_calls, runs the tools, and sends the history back sanitized`, async () => {
      const c = client(gateways.standard);
      const user = { role: "user" as const, content: "weather in Seoul and 서울?" };
      const first = (await c.chat.completions.create({ model, messages: [user], tools: [tool], tool_choice: "auto", parallel_tool_calls: results > 1 })) as Completion;
      const message = first.choices[0]!.message;
      assert.equal(first.choices[0]!.finish_reason, "tool_calls");
      assert.equal(message.tool_calls?.length, results);
      const assistant = historyFrom(message);
      const toolMessages = assistant.tool_calls.map((tc, i) => ({
        role: "tool" as const,
        tool_call_id: tc.id,
        content: `result ${i + 1}: sunny, key ${secret(10 + i)}`,
      }));
      const second = (await c.chat.completions.create({
        model: "qual-json-ok",
        messages: [user, assistant, ...toolMessages],
        tools: [tool],
      })) as Completion;
      assert.equal(second.choices[0]?.message.content, synthetic.stream_text);

      const { calls } = await provider.calls();
      assert.equal(calls.length, 2, "one request per SDK call");
      const sent = JSON.parse(calls[1]!.body) as { messages: Array<Record<string, unknown>> };
      // The assistant turn the provider produced is retained exactly (ids, names, argument trees).
      assert.deepEqual(normalize(sent as unknown as Json).normalized, normalize({
        model: "qual-json-ok",
        messages: [user, assistant, ...toolMessages.map((m, i) => ({ ...m, content: `result ${i + 1}: sunny, key <SECRET_${i + 1}>` }))],
        tools: [JSON.parse(JSON.stringify(tool))],
      } as unknown as Json).normalized);
      assert.deepEqual(plantedIn(calls[1]!.body), []);
      observations[`round_trip_${model}`] = { upstream_requests: 2, tool_results: results, placeholders: results };
    });
  }

  it("replaying the provider's message object verbatim is rejected (422): refusal and annotations are unknown fields; the documented fields alone pass", async () => {
    const c = client(gateways.standard);
    const user = { role: "user" as const, content: "q" };
    const first = (await c.chat.completions.create({ model: "qual-json-tool-call", messages: [user], tools: [tool] })) as Completion;
    const message = first.choices[0]!.message;
    assert.ok("refusal" in message, "the provider reply carries refusal: null like a real one");
    const e = await sdkError(() =>
      c.chat.completions.create({
        model: "qual-json-ok",
        messages: [user, message as never, { role: "tool", tool_call_id: message.tool_calls![0]!.id, content: "ok" }],
      }),
    );
    assert.equal(e.status, 422);
    assert.equal(e.code, "unsupported_input");
    assert.equal((await provider.calls()).calls.length, 1, "only the first request reached the provider");
    observations["verbatim_message_replay"] = "rejected 422 unsupported_input (refusal / annotations)";
  });
});

describe("request-state isolation under concurrency (Node SDK)", () => {
  beforeEach(async () => {
    await provider.reset();
  });

  it("placeholder numbering, tool history and sanitized bodies are per request when 24 requests run at once", async () => {
    const total = 24;
    interface Plan {
      i: number;
      marker: string;
      k: number;
      params: Record<string, unknown>;
      expected: Record<string, unknown>;
    }
    const plans: Plan[] = Array.from({ length: total }, (_unused, i) => {
      const marker = `${synthetic.prompt_marker_prefix}${randomUUID()}`;
      const k = (i % 4) + 1;
      const s = (j: number) => secret(1000 * (i + 1) + j);
      const make = (ph: (j: number) => string) => ({
        model: "qual-json-ok",
        messages: [
          { role: "user", content: `req ${i} ${marker} ${ph(1)}` },
          ...(k >= 2
            ? [
                { role: "assistant", content: null, tool_calls: [{ id: `call_${i}`, type: "function", function: { name: "lookup", arguments: JSON.stringify({ q: ph(2), n: i }) } }] },
                { role: "tool", tool_call_id: `call_${i}`, content: k >= 3 ? `found ${ph(3)}` : "found" },
              ]
            : []),
        ],
        ...(k >= 4 ? { metadata: { trace: `t-${i} ${ph(4)}` } } : {}),
      });
      return { i, marker, k, params: make(s), expected: make((j) => `<SECRET_${j}>`) };
    });
    // Rejected neighbours in flight at the same time: a label secret and a private key.
    const bad = [
      { model: "qual-json-ok", messages: [{ role: "user", content: "q" }], tools: [{ type: "function", function: { name: secret(7) } }] },
      { model: "qual-json-ok", messages: [{ role: "user", content: "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----" }] },
    ];
    const results = await Promise.allSettled([
      ...plans.map((p) => client(gateways.concurrent, { maxRetries: 4 }).chat.completions.create(p.params as unknown as Params)),
      ...bad.map((b) => client(gateways.concurrent, { maxRetries: 0 }).chat.completions.create(b as Params)),
    ]);
    plans.forEach((p, idx) => assert.equal(results[idx]!.status, "fulfilled", `request ${p.i} completed`));
    for (const r of results.slice(total)) {
      assert.equal(r.status, "rejected");
      assert.equal(((r as PromiseRejectedResult).reason as InstanceType<typeof OpenAI.APIError>).status, 422);
    }
    const { calls } = await provider.calls();
    assert.equal(calls.length, total, "one upstream request per forwarded request, none for the rejected neighbours");
    for (const p of plans) {
      const mine = calls.filter((c) => c.body.includes(p.marker));
      assert.equal(mine.length, 1, `request ${p.i}: exactly one upstream request`);
      const sent = JSON.parse(mine[0]!.body) as Json;
      assert.deepEqual(normalize(sent).normalized, normalize(p.expected as Json).normalized, `request ${p.i}: numbering restarts at 1 and no other request's text appears`);
      assert.equal(mine[0]!.body.split("<SECRET_").length - 1, p.k, `request ${p.i}: placeholder count`);
    }
    assert.deepEqual(plantedIn(calls.map((c) => c.body).join("")), []);
    observations["concurrency"] = { concurrent_forwarded: total, rejected_neighbours: bad.length, upstream_requests: calls.length };
  });
});
