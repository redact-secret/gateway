// Manual tool round trips and the SDK's structured-output and tool helpers against POST /v1/responses (#88).
//
// Every row sends a real SDK request through the gateway and compares the fake provider's RECORDED
// request, so "accepted" means the sanitized body reached the provider and "rejected" means zero
// connections. The helper table is MEASURED against the pinned `openai` 7.27.0 and `zod` 4.6.5; it
// replaces the type-surface review that the contract had (docs/contracts/responses-request.md).
import assert from "node:assert/strict";
import { after, beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { zodResponsesFunction, zodTextFormat } from "openai/helpers/zod";
import { z } from "zod";
import { assertNothingUpstream, client, gateways, leaks, provider, secret, synthetic, writeEvidence } from "./support.ts";

type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

const observations: Array<Record<string, unknown>> = [];
after(() => {
  writeEvidence("responses-tools-observations-node.json", { sdk: "openai (npm) 7.27.0", zod: "4.6.5", node: process.version, observations });
});

const weather = {
  type: "function" as const,
  name: "get_weather",
  description: "Current weather for a city.",
  parameters: {
    type: "object",
    properties: { city: { type: "string" }, days: { type: "integer" } },
    required: ["city", "days"],
    additionalProperties: false,
  },
  strict: true,
};

async function lastCall(): Promise<{ body: Json; path: string; raw: string }> {
  const { calls } = await provider.calls();
  assert.equal(calls.length > 0, true, "the provider received a request");
  const call = calls.at(-1)!;
  return { body: JSON.parse(call.body) as Json, path: call.path, raw: call.body };
}

describe("manual tool round trips (Node SDK, POST /v1/responses)", () => {
  beforeEach(provider.reset);

  for (const parallel of [false, true]) {
    it(`${parallel ? "parallel calls" : "one call"}: request, provider function_call, rebuilt history with the result, final answer`, async () => {
      const c = client(gateways.concurrent);
      const first = await c.responses.create({
        model: parallel ? "qual-resp-tool-calls-parallel" : "qual-resp-tool-call",
        input: `Weather for ${secret(1)}?`,
        store: false,
        tools: [weather],
        tool_choice: "auto",
        parallel_tool_calls: parallel,
      });
      const calls = first.output.filter((i): i is OpenAI.Responses.ResponseFunctionToolCall => i.type === "function_call");
      assert.equal(calls.length, parallel ? 2 : 1);
      const firstSent = await lastCall();
      assert.equal(firstSent.path, "/v1/responses");
      assert.ok(!firstSent.raw.includes("ghp_"));
      assert.deepEqual((firstSent.body as { input: Json }).input, "Weather for <SECRET_1>?");

      // The application rebuilds the history from the listed keys only (contract: safe replay conversion).
      const history: OpenAI.Responses.ResponseInput = [{ role: "user", content: `Weather for ${secret(1)}?` }];
      for (const call of calls) {
        history.push({ type: "function_call", call_id: call.call_id, name: call.name, arguments: call.arguments });
      }
      for (const call of calls) {
        history.push({ type: "function_call_output", call_id: call.call_id, output: JSON.stringify({ temp_c: 21, note: secret(2), city: JSON.parse(call.arguments).city }) });
      }
      const second = await c.responses.create({ model: "qual-resp-json-ok", input: history, store: false, tools: [weather] });
      assert.equal(second.output_text, synthetic.stream_text);

      const sent = (await lastCall()).body as { input: Array<Record<string, Json>>; store: Json; tools: Json[] };
      assert.equal(sent.store, false);
      assert.equal(sent.input.length, 1 + 2 * calls.length);
      assert.deepEqual(sent.input[0], { role: "user", content: "Weather for <SECRET_1>?" });
      for (const [i, call] of calls.entries()) {
        const sentCall = sent.input[1 + i]!;
        assert.deepEqual(Object.keys(sentCall).sort(), ["arguments", "call_id", "name", "type"], "only the listed keys were sent");
        assert.equal(sentCall["call_id"], call.call_id);
        assert.equal(sentCall["arguments"], call.arguments, "compact arguments are byte-identical after re-encoding");
        const result: Record<string, Json> = sent.input[1 + calls.length + i]!;
        assert.equal(result["type"], "function_call_output");
        assert.ok(String(result["output"]).includes("<SECRET_"), "the result text was redacted in place");
        assert.ok(!String(result["output"]).includes("ghp_"));
      }
      // Two provider deliveries, two connections, nothing else.
      const seen = await provider.calls();
      assert.equal(seen.calls.length, 2);
      assert.equal(seen.connections, 2);
      observations.push({ case: parallel ? "tool round trip, two calls" : "tool round trip, one call", provider_requests: 2, accepted: true });
    });
  }

  it("replaying response.output unchanged is rejected (provider ids, status) and sends nothing further", async () => {
    const c = client(gateways.concurrent);
    const first = await c.responses.create({ model: "qual-resp-tool-call", input: "weather?", store: false, tools: [weather] });
    assert.equal((await provider.calls()).calls.length, 1);
    let caught: unknown;
    try {
      await c.responses.create({
        model: "qual-resp-json-ok",
        store: false,
        input: [{ role: "user", content: "weather?" }, ...(first.output as OpenAI.Responses.ResponseInput), { type: "function_call_output", call_id: "call_qual_1", output: "sunny" }],
      });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.APIError);
    assert.equal(caught.status, 422);
    assert.equal(caught.code, "unsupported_input");
    assert.equal((await provider.calls()).calls.length, 1, "the rejected replay delivered nothing");
    observations.push({ case: "replay response.output verbatim", status: 422, code: "unsupported_input" });
  });

  it("an output for a call that was never made is refused before any upstream contact", async () => {
    let caught: unknown;
    try {
      await client(gateways.concurrent).responses.create({
        model: "qual-resp-json-ok",
        store: false,
        input: [{ type: "function_call_output", call_id: "call_never", output: "x" }],
      });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof OpenAI.APIError && caught.status === 422);
    await assertNothingUpstream(assert, "linkage");
  });
});

// ---------------------------------------------------------------------------------------------
// SDK helpers, measured.
const Flat = z.object({ city: z.string().describe("City name"), days: z.number() });
const Nested = z.object({ place: z.object({ city: z.string(), country: z.string() }), days: z.number() });
const WithEnum = z.object({ unit: z.enum(["c", "f"]), city: z.string() });
const WithNullable = z.object({ city: z.string(), note: z.string().nullable() });
const WithArray = z.object({ cities: z.array(z.object({ name: z.string() })) });
const WithEmail = z.object({ contact: z.string().email() });
const WithRegex = z.object({ code: z.string().regex(/^[A-Z]{3}$/) });
const WithDefault = z.object({ city: z.string().default("Seoul") });

type TextFormat = ReturnType<typeof zodTextFormat>;
function dropSchemaKey(format: TextFormat): TextFormat {
  const copy = JSON.parse(JSON.stringify(format)) as { schema: Record<string, unknown> };
  delete copy.schema["$schema"];
  return copy as unknown as TextFormat;
}
function dropFunctionSchemaKey<T extends { parameters: unknown }>(tool: T): T {
  const copy = JSON.parse(JSON.stringify(tool)) as T & { parameters: Record<string, unknown> };
  delete copy.parameters["$schema"];
  return copy;
}

const probes: Array<{ id: string; accepted: boolean; why: string; run: (c: OpenAI) => Promise<unknown> }> = [
  { id: "zodTextFormat flat, as emitted", accepted: false, why: "$schema is written", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: zodTextFormat(Flat, "answer") } }) },
  { id: "zodTextFormat flat, $schema deleted", accepted: true, why: "only the rejected key removed", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(Flat, "answer")) } }) },
  { id: "zodTextFormat nested object, $schema deleted", accepted: true, why: "zod inlines nested objects", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(Nested, "answer")) } }) },
  { id: "zodTextFormat z.enum, $schema deleted", accepted: true, why: "enum strings are inspected labels", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithEnum, "answer")) } }) },
  { id: "zodTextFormat nullable string, $schema deleted", accepted: true, why: "type array with null is in the subset", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithNullable, "answer")) } }) },
  { id: "zodTextFormat array of objects, $schema deleted", accepted: true, why: "items objects are in the subset", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithArray, "answer")) } }) },
  { id: "zodTextFormat .email(), $schema deleted", accepted: false, why: "format keyword", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithEmail, "answer")) } }) },
  { id: "zodTextFormat .regex(), $schema deleted", accepted: false, why: "pattern keyword", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithRegex, "answer")) } }) },
  { id: "zodTextFormat .default(), $schema deleted", accepted: false, why: "default keyword", run: (c) => c.responses.parse({ model: "qual-resp-json-structured", input: "q", store: false, text: { format: dropSchemaKey(zodTextFormat(WithDefault, "answer")) } }) },
  { id: "zodResponsesFunction flat, as emitted", accepted: false, why: "$schema is written", run: (c) => c.responses.parse({ model: "qual-resp-tool-call", input: "q", store: false, tools: [zodResponsesFunction({ name: "get_weather", parameters: Flat, description: "Weather" })] }) },
  { id: "zodResponsesFunction flat, $schema deleted", accepted: true, why: "only the rejected key removed", run: (c) => c.responses.parse({ model: "qual-resp-tool-call", input: "q", store: false, tools: [dropFunctionSchemaKey(zodResponsesFunction({ name: "get_weather", parameters: Flat, description: "Weather" }))] }) },
  { id: "zodResponsesFunction without description, $schema deleted", accepted: true, why: "an absent description is omitted, not null", run: (c) => c.responses.parse({ model: "qual-resp-tool-call", input: "q", store: false, tools: [dropFunctionSchemaKey(zodResponsesFunction({ name: "get_weather", parameters: Flat }))] }) },
  { id: "hand-built flat function tool", accepted: true, why: "the documented flat shape", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [weather] }) },
  { id: "hand-built flat tool with parameters:null and strict:null", accepted: true, why: "both required keys present", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [{ type: "function", name: "ping", description: "d", parameters: null, strict: null }] }) },
  { id: "Chat-shaped tool {type, function:{...}}", accepted: false, why: "flat tool has no function wrapper", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [{ type: "function", function: { name: "t", parameters: { type: "object" } } } as never] }) },
  { id: "Chat-shaped response_format via extra field", accepted: false, why: "unknown top-level field", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, response_format: { type: "json_object" } } as never) },
  { id: "SDK default call without store", accepted: false, why: "store is required to be false and the SDK does not send it", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q" }) },
  { id: "tool_choice in the Chat shape", accepted: false, why: "flat form only", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [weather], tool_choice: { type: "function", function: { name: "get_weather" } } as never }) },
  { id: "tool_choice flat function form", accepted: true, why: "documented form", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [weather], tool_choice: { type: "function", name: "get_weather" } }) },
  { id: "hosted web_search tool", accepted: false, why: "hosted tools are rejected", run: (c) => c.responses.create({ model: "qual-resp-json-ok", input: "q", store: false, tools: [{ type: "web_search" }] }) },
];

describe("SDK helper output against the Responses subset (Node SDK, measured)", () => {
  beforeEach(provider.reset);
  for (const p of probes) {
    it(`${p.accepted ? "accepted" : "rejected"}: ${p.id} (${p.why})`, async () => {
      let status = 200;
      let code: string | null = null;
      let rendered = "";
      let result: unknown;
      try {
        result = await p.run(client(gateways.concurrent));
      } catch (e) {
        assert.ok(e instanceof OpenAI.APIError, String(e));
        status = e.status ?? 0;
        code = e.code ?? null;
        rendered = JSON.stringify({ m: e.message, e: e.error });
      }
      const seen = await provider.calls();
      assert.deepEqual(leaks(rendered), []);
      if (p.accepted) {
        assert.equal(status, 200, `${p.id}: ${code ?? ""}`);
        assert.equal(seen.calls.length, 1);
        assert.equal(seen.calls[0]!.path, "/v1/responses");
        // The SDK's parse helper type-checked the relayed reply.
        if (p.id.startsWith("zodTextFormat")) assert.deepEqual((result as { output_parsed: unknown }).output_parsed !== null, true);
      } else {
        assert.ok(status === 422, `${p.id}: expected a 422, got ${status}`);
        assert.equal(code, "unsupported_input");
        await assertNothingUpstream(assert, p.id);
      }
      observations.push({ helper: p.id, accepted: p.accepted, status, code, provider_requests: seen.calls.length });
    });
  }

  it("zodTextFormat description text is redacted in the forwarded schema and the parse helper returns the typed value", async () => {
    const Described = z.object({ city: z.string().describe(`The city ${secret(7)}`), days: z.number() });
    const reply = await client(gateways.concurrent).responses.parse({
      model: "qual-resp-json-structured",
      input: "q",
      store: false,
      text: { format: dropSchemaKey(zodTextFormat(Described, "answer")) },
    });
    assert.deepEqual(reply.output_parsed, { city: "Seoul", days: 3 });
    const sent = (await lastCall()).body as { text: { format: { schema: { properties: { city: { description: string } } } } } };
    assert.equal(sent.text.format.schema.properties.city.description, "The city <SECRET_1>");
  });

  it("zodResponsesFunction: the parse helper returns typed arguments from the relayed function_call", async () => {
    const reply = await client(gateways.concurrent).responses.parse({
      model: "qual-resp-tool-call",
      input: "q",
      store: false,
      tools: [dropFunctionSchemaKey(zodResponsesFunction({ name: "get_weather", parameters: Flat, description: "Weather" }))],
    });
    const call = reply.output.find((i) => i.type === "function_call") as { parsed_arguments?: unknown } | undefined;
    assert.deepEqual(call?.parsed_arguments, { city: "Seoul", days: 3 });
  });
});
