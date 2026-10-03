// Alpha 2 field coverage and policy outcomes through the pinned OpenAI Node SDK (#57).
//
// One test per row of qualification/alpha2-cases.json (shared with the Python harness). Forwarded
// rows are checked against the fake provider's RECORDED request: the parsed body must equal the
// expected sanitized body (every retained structure, not just HTTP 200), the planted secrets must
// be absent, the placeholder count must match, and exactly one request must have been delivered.
// Rejected rows must show the documented status and safe code, no secret-bearing error text, and
// ZERO provider connections and requests.
import assert from "node:assert/strict";
import { after, describe, it } from "node:test";
import OpenAI from "openai";
import { assertNothingUpstream, client, leaks, provider, synthetic, writeEvidence } from "./support.ts";
import {
  cases,
  expectedBody,
  gatewayFor,
  normalize,
  placeholderCount,
  plainParams,
  plantedIn,
  rawBytes,
  rawPost,
  type Case,
} from "./alpha2-support.ts";

type Params = Parameters<OpenAI["chat"]["completions"]["create"]>[0];

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

async function runCase(c: Case, gateway: string): Promise<void> {
  await provider.reset();
  const base = gatewayFor[gateway];
  assert.ok(base, `unknown gateway ${gateway}`);
  const viaSdk = c.params !== undefined;
  let status: number | null = null;
  let code: string | null = null;
  let rendered = "";
  let completed = false;
  if (viaSdk) {
    try {
      const res = (await client(base).chat.completions.create(plainParams(c) as unknown as Params)) as OpenAI.Chat.Completions.ChatCompletion;
      completed = res.choices[0]?.message.content === synthetic.stream_text;
      status = 200;
    } catch (e) {
      assert.ok(e instanceof OpenAI.APIError, `${c.id}: expected an API error, got ${String(e)}`);
      status = e.status ?? null;
      code = e.code ?? null;
      rendered = JSON.stringify({ m: e.message, e: e.error, h: [...(e.headers?.entries?.() ?? [])] });
    }
  } else {
    const r = await rawPost(base, rawBytes(c), synthetic.api_key);
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
  const call = seen.calls[0]!;
  const sent = JSON.parse(call.body) as never;
  const want = expectedBody(c);
  const got = normalize(sent);
  const exp = normalize(want);
  assert.deepEqual(got.normalized, exp.normalized, `${c.id}@${gateway}: sanitized body differs from the expected one`);
  record.placeholders = placeholderCount(sent);
  assert.equal(record.placeholders, placeholderCount(want), `${c.id}@${gateway}: placeholder count`);
  if (!c.plaintext_forwarded) {
    assert.deepEqual(plantedIn(call.body), [], `${c.id}@${gateway}: a planted secret reached the provider`);
  }
  if (c.exact_arguments) {
    assert.deepEqual(got.argumentStrings, c.exact_arguments, `${c.id}@${gateway}: re-encoded arguments strings`);
  }
}

for (const group of [...new Set(cases.map((c) => c.group))]) {
  describe(`Alpha 2 cases: ${group} (Node SDK)`, () => {
    for (const c of cases.filter((x) => x.group === group)) {
      for (const gateway of c.gateways ?? ["standard"]) {
        it(`${c.id} [${gateway}] -> ${c.expect.kind === "forward" ? "forwarded sanitized" : `${c.expect.status} ${c.expect.code}`}`, async () => {
          await runCase(c, gateway);
        });
      }
    }
  });
}

after(() => {
  writeEvidence("alpha2-cases-node.json", {
    sdk: "openai (npm) 7.27.0",
    node: process.version,
    cases: rows.length,
    forwarded: rows.filter((r) => r.outcome === "forward").length,
    rejected: rows.filter((r) => r.outcome === "reject").length,
    rejected_with_upstream_traffic: rows.filter((r) => r.outcome === "reject" && (r.provider_requests > 0 || r.provider_connections > 0)).length,
    rows,
  });
});
