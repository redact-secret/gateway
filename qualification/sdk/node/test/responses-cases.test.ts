// Responses field matrix and policy outcomes through the pinned OpenAI Node SDK (#88).
//
// One test per row of qualification/responses-cases.json (shared with the Python harness). Forwarded
// rows are checked against the fake provider's RECORDED request: the parsed body must equal the
// expected sanitized body (every retained structure, not just HTTP 200), the planted secrets must
// be absent, the placeholder count must match, exactly one request on exactly one connection must
// have been delivered, to /v1/responses, with the provider credential and no local token. Rejected
// rows must show the documented status and safe code, no secret-bearing error text, and ZERO
// provider connections and requests. A coverage test pins the slot list of the contract matrix.
import assert from "node:assert/strict";
import { after, describe, it } from "node:test";
import OpenAI from "openai";
import { placeholderCount, plantedIn } from "./alpha2-support.ts";
import { rawPostTo, expectedBody, normalize, plainParams, rawBytes, rcases, rgatewayFor, type RCase } from "./responses-support.ts";
import { assertNothingUpstream, authSha, client, leaks, provider, synthetic, writeEvidence } from "./support.ts";

type Params = Parameters<OpenAI["responses"]["create"]>[0];

interface Row {
  id: string;
  group: string;
  gateway: string;
  path: "sdk" | "raw";
  outcome: string;
  status: number | null;
  provider_requests: number;
  provider_connections: number;
  placeholders: number;
}
const rows: Row[] = [];

async function runCase(c: RCase, gateway: string): Promise<void> {
  await provider.reset();
  const base = rgatewayFor[gateway];
  assert.ok(base, `unknown gateway ${gateway}`);
  const viaSdk = c.params !== undefined;
  let status: number | null = null;
  let code: string | null = null;
  let rendered = "";
  let completed = false;
  if (viaSdk) {
    try {
      const out = await client(base).responses.create(plainParams(c) as unknown as Params);
      if (c.stream) {
        let terminal = false;
        for await (const event of out as AsyncIterable<{ type: string }>) terminal ||= event.type === "response.completed";
        completed = terminal;
      } else {
        completed = (out as OpenAI.Responses.Response).output_text === synthetic.stream_text;
      }
      status = 200;
    } catch (e) {
      assert.ok(e instanceof OpenAI.APIError, `${c.id}: expected an API error, got ${String(e)}`);
      status = e.status ?? null;
      code = e.code ?? null;
      rendered = JSON.stringify({ m: e.message, e: e.error, h: [...(e.headers?.entries?.() ?? [])] });
    }
  } else {
    const r = await rawPostTo(base, c.endpoint ?? "responses", rawBytes(c), synthetic.api_key);
    status = r.status;
    code = r.code;
    rendered = r.text;
    completed = r.status === 200;
  }
  const seen = await provider.calls();
  const record: Row = {
    id: c.id,
    group: c.group,
    gateway,
    path: viaSdk ? "sdk" : "raw",
    outcome: c.expect.kind,
    status,
    provider_requests: seen.calls.length,
    provider_connections: seen.connections,
    placeholders: 0,
  };
  rows.push(record);

  if (c.expect.kind === "reject") {
    assert.equal(status, c.expect.status, `${c.id}@${gateway}: status`);
    assert.equal(code, c.expect.code, `${c.id}@${gateway}: safe error code`);
    assert.deepEqual(leaks(rendered), [], `${c.id}@${gateway}: error text leaks request content`);
    assert.deepEqual(plantedIn(rendered), [], `${c.id}@${gateway}: error text carries a secret marker`);
    await assertNothingUpstream(assert, `${c.id}@${gateway}`);
    return;
  }

  assert.equal(status, 200, `${c.id}@${gateway}: forwarded`);
  assert.ok(completed, `${c.id}@${gateway}: the provider reply was relayed`);
  assert.equal(seen.calls.length, 1, `${c.id}@${gateway}: exactly one upstream request`);
  assert.equal(seen.connections, 1, `${c.id}@${gateway}: exactly one upstream connection`);
  const call = seen.calls[0]!;
  assert.equal(call.method, "POST");
  assert.equal(call.path, "/v1/responses", `${c.id}@${gateway}: the Responses destination, not the Chat one`);
  assert.equal(call.authorization_sha256, authSha(), `${c.id}@${gateway}: the caller's provider credential, unchanged`);
  assert.equal(call.local_token_seen, false);
  const sent = JSON.parse(call.body) as never;
  const want = expectedBody(c);
  const got = normalize(sent);
  const exp = normalize(want);
  assert.deepEqual(got.normalized, exp.normalized, `${c.id}@${gateway}: sanitized body differs from the expected one`);
  record.placeholders = placeholderCount(sent);
  assert.equal(record.placeholders, placeholderCount(want), `${c.id}@${gateway}: placeholder count`);
  assert.equal((sent as { store?: unknown }).store, false, `${c.id}@${gateway}: store:false is always written`);
  if (!c.plaintext_forwarded) {
    assert.deepEqual(plantedIn(call.body), [], `${c.id}@${gateway}: a planted secret reached the provider`);
  }
  if (c.exact_arguments) {
    assert.deepEqual(got.argumentStrings, c.exact_arguments, `${c.id}@${gateway}: re-encoded arguments strings`);
  }
}

for (const group of [...new Set(rcases.map((c) => c.group))]) {
  describe(`Responses cases: ${group} (Node SDK)`, () => {
    for (const c of rcases.filter((x) => x.group === group)) {
      for (const gateway of c.gateways ?? ["standard"]) {
        it(`${c.id} [${gateway}] -> ${c.expect.kind === "forward" ? "forwarded sanitized" : `${c.expect.status} ${c.expect.code}`}`, async () => {
          await runCase(c, gateway);
        });
      }
    }
  });
}

// The slots the contract matrix lists. Every text slot has a forwarded-and-redacted row, every label
// slot a rejected row that planted a secret there, and every structural field a row showing that it
// cannot carry text. A new matrix row without a case fails here.
const TEXT_SLOTS = [
  "instructions", "input.string", "input.message.content.string", "input.message.content.parts", "function_call.arguments.leaf",
  "function_call_output.output.string", "function_call_output.output.parts", "tools.description", "tools.schema.description", "tools.schema.title",
  "text.format.description", "text.format.schema.description", "text.format.schema.title", "metadata.value",
];
const LABEL_SLOTS = [
  "model", "function-call-call-id", "function-call-name", "function-call-output-call-id", "function-call-arguments-key", "tool-name",
  "tool-schema-property-key", "tool-schema-required-entry", "tool-schema-enum-string", "tool-schema-const-string", "tool-choice-name",
  "text-format-name", "text-format-schema-property-key", "text-format-schema-enum-string", "metadata-key",
];
const STRUCTURAL = ["role", "message-type", "part-type", "phase", "item-type", "tool-type", "tool-choice-string", "tool-choice-type", "text-format-type", "verbosity", "stream-options-key"];

describe("Responses matrix coverage (Node SDK)", () => {
  it("every text slot has a forwarded row that redacted it", () => {
    for (const slot of TEXT_SLOTS) {
      assert.ok(rcases.some((c) => c.expect.kind === "forward" && c.slots.includes(slot) && Object.keys(c.expect.map ?? {}).length > 0), `no redaction row for ${slot}`);
    }
  });
  it("every label slot has a rejected row that planted a secret in it", () => {
    for (const slot of LABEL_SLOTS) {
      assert.ok(rcases.some((c) => c.expect.kind === "reject" && c.slots.includes(`label.${slot}`)), `no label row for ${slot}`);
    }
  });
  it("every structural field has a rejected row that planted a secret in it", () => {
    for (const slot of STRUCTURAL) {
      assert.ok(rcases.some((c) => c.expect.kind === "reject" && c.slots.includes(`structural.${slot}`)), `no structural row for ${slot}`);
    }
  });
  it("every row that plants a secret and is rejected never reaches the provider (checked in the rows)", () => {
    assert.ok(rcases.filter((c) => c.expect.kind === "reject").length > 150);
  });
});

after(() => {
  writeEvidence("responses-cases-node.json", {
    sdk: "openai (npm) 7.27.0",
    node: process.version,
    cases: rows.length,
    forwarded: rows.filter((r) => r.outcome === "forward").length,
    rejected: rows.filter((r) => r.outcome === "reject").length,
    rejected_with_upstream_traffic: rows.filter((r) => r.outcome === "reject" && (r.provider_requests > 0 || r.provider_connections > 0)).length,
    rows,
  });
});
