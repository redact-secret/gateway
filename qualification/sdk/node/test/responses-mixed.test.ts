// Both endpoints under concurrent mixed load with request-local state and independent provider
// credentials (#88). 28 requests at once per wave against ONE gateway instance, alternating
// POST /v1/chat/completions and POST /v1/responses, mixing redacted, blocked, warn-rejected,
// label-rejected, provider-declared incomplete and streamed outcomes. Every request carries a
// unique marker and its OWN provider key. The oracle is the fake provider's record: each forwarded
// request must appear exactly once, on its own endpoint path and connection, with its own key's
// Authorization hash, its own secrets numbered from <SECRET_1> and nothing of any neighbour; every
// rejected request must be absent from the record. Status 200 alone is not an oracle.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { after, beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { gateways, leaks, promptMarker, provider, secret, synthetic, writeEvidence, type Call } from "./support.ts";

const PEM = "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----";
const WARN = "password=hunter2xyz";

type Kind =
  | "chat-redact" | "chat-stream" | "chat-block" | "chat-warn"
  | "resp-redact" | "resp-tool-history" | "resp-stream" | "resp-stream-incomplete" | "resp-incomplete-json"
  | "resp-block" | "resp-warn" | "resp-label" | "resp-store-missing";

interface Job {
  i: number;
  kind: Kind;
  marker: string;
  key: string;
  secrets: number[];
  forwards: boolean;
  path: "/v1/chat/completions" | "/v1/responses";
}

const FORWARDING: Kind[] = ["chat-redact", "chat-stream", "resp-redact", "resp-tool-history", "resp-stream", "resp-stream-incomplete", "resp-incomplete-json"];
// A fixed 28-job pattern per wave: 16 forwarded (9 Responses, 7 Chat) and 12 rejected, interleaved.
const PATTERN: Kind[] = [
  "resp-redact", "chat-redact", "resp-block", "resp-tool-history", "chat-block", "resp-stream", "resp-label",
  "chat-stream", "resp-warn", "resp-incomplete-json", "chat-warn", "resp-redact", "resp-store-missing", "chat-redact",
  "resp-stream-incomplete", "resp-block", "chat-redact", "resp-tool-history", "resp-label", "chat-stream", "resp-warn",
  "resp-redact", "chat-block", "resp-incomplete-json", "resp-stream", "chat-warn", "resp-store-missing", "chat-redact",
];

let counter = 100;
function makeJobs(): Job[] {
  return PATTERN.map((kind, i) => {
    const forwards = FORWARDING.includes(kind);
    const n = 1 + (i % 3); // 1 to 3 distinct secrets per request
    return {
      i,
      kind,
      marker: promptMarker(),
      key: `${synthetic.api_key}-${i}`,
      secrets: Array.from({ length: n }, () => counter++),
      forwards,
      path: kind.startsWith("chat") ? "/v1/chat/completions" : "/v1/responses",
    };
  });
}

const sha = (key: string): string => createHash("sha256").update(`Bearer ${key}`).digest("hex");
const text = (j: Job): string => `${j.marker} ${j.secrets.map((s) => `tok ${secret(s)}`).join(" ")}`;

async function send(j: Job): Promise<{ status: number; ok: boolean; note?: string }> {
  const c = new OpenAI({ baseURL: `${gateways.concurrent}/v1`, apiKey: j.key, maxRetries: 4 });
  // Bounded admission may answer 503 overload to a burst; such a request never reached the provider and
  // the SDK retries it after the relayed wait, so the delivery count below stays exact.
  try {
    switch (j.kind) {
      case "chat-redact":
        return { status: 200, ok: (await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: text(j) }] })).choices[0]?.message.content === synthetic.stream_text };
      case "chat-stream": {
        const s = await c.chat.completions.create({ model: "qual-sse-ok", messages: [{ role: "user", content: text(j) }], stream: true });
        let finish: string | null = null;
        for await (const chunk of s) finish = chunk.choices[0]?.finish_reason ?? finish;
        return { status: 200, ok: finish === "stop" };
      }
      case "chat-block":
        await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: `${j.marker} ${PEM}` }] });
        return { status: 200, ok: false };
      case "chat-warn":
        await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: `${j.marker} ${WARN}` }] });
        return { status: 200, ok: false };
      case "resp-redact":
        return { status: 200, ok: (await c.responses.create({ model: "qual-resp-json-ok", input: text(j), instructions: `i ${j.marker}`, store: false })).output_text === synthetic.stream_text };
      case "resp-tool-history":
        return {
          status: 200,
          ok: (await c.responses.create({
            model: "qual-resp-json-ok",
            store: false,
            input: [
              { role: "user", content: text(j) },
              { type: "function_call", call_id: `call_${j.i}`, name: "lookup", arguments: JSON.stringify({ q: j.marker }) },
              { type: "function_call_output", call_id: `call_${j.i}`, output: "result" },
            ],
          })).output_text === synthetic.stream_text,
        };
      case "resp-stream":
      case "resp-stream-incomplete": {
        const s = await c.responses.create({ model: j.kind === "resp-stream" ? "qual-resp-sse-ok" : "qual-resp-sse-incomplete", input: text(j), store: false, stream: true });
        let terminal: string | null = null;
        for await (const e of s) if (e.type.startsWith("response.") && ["completed", "incomplete", "failed"].includes(e.type.slice(9))) terminal = e.type.slice(9);
        return { status: 200, ok: terminal === (j.kind === "resp-stream" ? "completed" : "incomplete") };
      }
      case "resp-incomplete-json":
        return { status: 200, ok: (await c.responses.create({ model: "qual-resp-json-incomplete", input: text(j), store: false })).status === "incomplete" };
      case "resp-block":
        await c.responses.create({ model: "qual-resp-json-ok", input: `${j.marker} ${PEM}`, store: false });
        return { status: 200, ok: false };
      case "resp-warn":
        await c.responses.create({ model: "qual-resp-json-ok", input: `${j.marker} ${WARN}`, store: false });
        return { status: 200, ok: false };
      case "resp-label":
        await c.responses.create({ model: "qual-resp-json-ok", input: j.marker, store: false, tools: [{ type: "function", name: secret(j.secrets[0]!), parameters: null, strict: null }] });
        return { status: 200, ok: false };
      case "resp-store-missing":
        await c.responses.create({ model: "qual-resp-json-ok", input: j.marker });
        return { status: 200, ok: false };
    }
  } catch (e) {
    assert.ok(e instanceof OpenAI.APIError, `${j.kind}: ${String(e)}`);
    assert.deepEqual(leaks(JSON.stringify({ m: e.message, e: e.error })), [], `${j.kind}: error text leaks`);
    return { status: e.status ?? 0, ok: false };
  }
}

const rows: Array<Record<string, unknown>> = [];

describe("both endpoints under concurrent mixed load (Node SDK)", () => {
  beforeEach(provider.reset);

  for (const wave of [1, 2]) {
    it(`wave ${wave}: 28 concurrent mixed requests, per-request state and credentials, provider record as the oracle`, async () => {
      const jobs = makeJobs();
      const results = await Promise.all(jobs.map((j) => send(j)));
      const { calls, connections } = await provider.calls();
      const forwarded = jobs.filter((j) => j.forwards);
      assert.equal(calls.length, forwarded.length, "exactly one delivery per forwarded request, none for rejected ones");
      assert.equal(connections, forwarded.length, "one upstream connection per delivery");

      const mine = (j: Job): Call[] => calls.filter((c) => c.body.includes(j.marker));
      for (const [idx, j] of jobs.entries()) {
        const r = results[idx]!;
        if (!j.forwards) {
          assert.equal(mine(j).length, 0, `${j.kind} #${j.i} reached the provider`);
          assert.ok(r.status === 422, `${j.kind} #${j.i}: status ${r.status}`);
          continue;
        }
        assert.ok(r.ok, `${j.kind} #${j.i}: the provider's outcome was relayed`);
        const seen = mine(j);
        assert.equal(seen.length, 1, `${j.kind} #${j.i}: delivered exactly once`);
        const call = seen[0]!;
        assert.equal(call.path, j.path, `${j.kind} #${j.i}: own endpoint`);
        assert.equal(call.authorization_sha256, sha(j.key), `${j.kind} #${j.i}: own provider credential`);
        assert.ok(!call.body.includes("ghp_SYNTH"), `${j.kind} #${j.i}: a secret reached the provider`);
        const placeholders = [...call.body.matchAll(/<SECRET_(\d+)>/g)].map((m) => Number(m[1]));
        if (j.kind === "resp-tool-history" || j.kind.endsWith("redact") || j.kind.endsWith("stream") || j.kind === "resp-incomplete-json" || j.kind === "resp-stream-incomplete") {
          assert.deepEqual([...new Set(placeholders)].sort((a, b) => a - b), j.secrets.map((_, k) => k + 1), `${j.kind} #${j.i}: numbering starts at 1 for this request alone`);
        }
        // No neighbour's marker or secret leaked into this body.
        for (const other of jobs) if (other !== j) assert.ok(!call.body.includes(other.marker), `${j.kind} #${j.i} carries marker of #${other.i}`);
      }
      // Distinct requests, distinct credentials: no two deliveries share an Authorization hash.
      assert.equal(new Set(calls.map((c) => c.authorization_sha256)).size, calls.length);
      assert.ok(calls.every((c) => !c.local_token_seen));
      const byPath = (p: string): number => calls.filter((c) => c.path === p).length;
      rows.push({ wave, requests: jobs.length, forwarded: forwarded.length, rejected: jobs.length - forwarded.length, chat_deliveries: byPath("/v1/chat/completions"), responses_deliveries: byPath("/v1/responses"), connections });
    });
  }

  it("after the load the gateway still serves both endpoints", async () => {
    const c = new OpenAI({ baseURL: `${gateways.concurrent}/v1`, apiKey: synthetic.api_key, maxRetries: 0 });
    assert.equal((await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: "after" }] })).choices[0]?.message.content, synthetic.stream_text);
    assert.equal((await c.responses.create({ model: "qual-resp-json-ok", input: "after", store: false })).output_text, synthetic.stream_text);
  });
});

after(() => {
  writeEvidence("responses-mixed-node.json", { sdk: "openai (npm) 7.27.0", node: process.version, waves: rows });
});
