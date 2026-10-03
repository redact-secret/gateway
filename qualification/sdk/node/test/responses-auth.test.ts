// Caller and provider credentials are independent authorities on BOTH endpoints (#88, extending
// #65). The same two authenticated instances as local-auth.test.ts (`authfile`: token file, full
// profile; `authenv`: token environment variable, common profile with on_warn forward) are driven
// through POST /v1/responses with the pinned SDK and raw HTTP, and through Chat and Responses
// interleaved on one instance. Oracle: the fake provider's own record (`local_token_seen`, header
// names, the Authorization hash, connections and requests).
import assert from "node:assert/strict";
import net from "node:net";
import { beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { assertNothingUpstream, authSha, client, gateways, leaks, provider, secret, synthetic, writeEvidence } from "./support.ts";

const LOCAL = "X-Gateway-Local-Token";
const instances = [
  { name: "authfile", base: gateways.authFile, redacts: true },
  { name: "authenv", base: gateways.authEnv, redacts: false },
] as const;
const evidence: Array<Record<string, unknown>> = [];

function rawPost(base: string, target: string, headerLines: string[], payload: string, declaredLength: number = Buffer.byteLength(payload)): Promise<{ status: number; text: string }> {
  const { hostname, port } = new URL(base);
  return new Promise((resolve, reject) => {
    const socket = net.connect({ host: hostname, port: Number(port) });
    const chunks: Buffer[] = [];
    let settled = false;
    const finish = (): void => {
      if (settled) return;
      settled = true;
      const raw = Buffer.concat(chunks).toString("utf8");
      const m = /^HTTP\/1\.1 (\d{3})/.exec(raw);
      if (!m) return reject(new Error("no HTTP response"));
      resolve({ status: Number(m[1]), text: raw.slice(raw.indexOf("\r\n\r\n") + 4) });
    };
    socket.on("data", (c) => chunks.push(c));
    socket.on("end", finish);
    socket.on("close", finish);
    socket.on("error", finish);
    socket.setTimeout(10_000, () => socket.destroy());
    socket.on("connect", () => {
      const head = [`POST ${target} HTTP/1.1`, `Host: ${hostname}:${port}`, "Content-Type: application/json", `Content-Length: ${declaredLength}`, "Connection: close", ...headerLines, "", ""].join("\r\n");
      socket.write(head + payload);
    });
  });
}

const provHeader = `Authorization: Bearer ${synthetic.api_key}`;
const respBody = (input = "hello"): string => JSON.stringify({ model: "qual-resp-json-ok", input, store: false });
const chatBody = (content = "hello"): string => JSON.stringify({ model: "qual-json-ok", messages: [{ role: "user", content }] });
const R = "/v1/responses";
const C = "/v1/chat/completions";

async function expectApiError(p: Promise<unknown>, status: number, code: string, why: string): Promise<void> {
  let caught: unknown;
  try {
    await p;
  } catch (e) {
    caught = e;
  }
  assert.ok(caught instanceof OpenAI.APIError, `${why}: expected an API error`);
  assert.equal(caught.status, status, `${why}: status`);
  assert.equal(caught.code, code, `${why}: safe code`);
  assert.deepEqual(leaks(JSON.stringify({ m: caught.message, e: caught.error, h: [...(caught.headers?.entries?.() ?? [])] })), [], `${why}: SDK-visible error exposes a secret value`);
}

for (const inst of instances) {
  describe(`Responses and local caller authentication on ${inst.name} (${inst.redacts ? "full profile" : "common profile, on_warn forward"})`, () => {
    beforeEach(provider.reset);
    const withToken = (): OpenAI => client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token } });

    it("both credentials: the provider gets the provider key on /v1/responses and no trace of the local token", async () => {
      const reply = await withToken().responses.create({ model: "qual-resp-json-ok", input: `token ${secret(1)}`, instructions: "be brief", store: false });
      assert.equal(reply.output_text, synthetic.stream_text);
      const { calls, connections } = await provider.calls();
      assert.deepEqual([calls.length, connections], [1, 1]);
      const call = calls[0]!;
      assert.equal(call.path, R);
      assert.equal(call.authorization_sha256, authSha(), "provider key forwarded unchanged");
      assert.ok(!call.header_names.includes(LOCAL.toLowerCase()), "the local header is not forwarded");
      assert.equal(call.local_token_seen, false, "the local token (or decoy) appeared in a header, the target or the body");
      if (inst.redacts) assert.ok(JSON.parse(call.body).input === "token <SECRET_1>");
      else assert.ok(JSON.parse(call.body).input === `token ${secret(1)}`, "common profile: the GitHub-style token has no detector and passes unchanged (Alpha 2 observation)");
      evidence.push({ gateway: inst.name, endpoint: "responses", check: "both credentials", status: 200, upstream_calls: 1, local_token_seen: false });
    });

    it("a streamed Responses request carries the same separation and ends on the provider's terminal event", async () => {
      const stream = await withToken().responses.create({ model: "qual-resp-sse-ok", input: "hello", store: false, stream: true });
      let terminal: string | null = null;
      for await (const e of stream) if (e.type === "response.completed") terminal = e.type;
      assert.equal(terminal, "response.completed");
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      assert.equal(calls[0]!.local_token_seen, false);
      assert.equal(calls[0]!.path, R);
    });

    it("one token and one provider key serve both endpoints; each delivery lands on its own path", async () => {
      const c = withToken();
      const chat = await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: `chat ${secret(2)}` }] });
      const resp = await c.responses.create({ model: "qual-resp-json-ok", input: `resp ${secret(3)}`, store: false });
      assert.equal(chat.choices[0]?.message.content, synthetic.stream_text);
      assert.equal(resp.output_text, synthetic.stream_text);
      const { calls, connections } = await provider.calls();
      assert.deepEqual(calls.map((x) => x.path), [C, R]);
      assert.equal(connections, 2);
      for (const call of calls) {
        assert.equal(call.authorization_sha256, authSha());
        assert.equal(call.local_token_seen, false);
        assert.ok(!call.header_names.includes(LOCAL.toLowerCase()));
      }
    });

    const rejected: Array<[string, () => OpenAI, number, string]> = [
      ["no local token", () => client(inst.base), 401, "local_auth_required"],
      ["the provider key sent as the local token", () => client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.api_key } }), 401, "local_auth_invalid"],
      ["a different well-formed token", () => client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token_decoy } }), 401, "local_auth_invalid"],
      ["the local token sent only as the provider key", () => client(inst.base, { apiKey: synthetic.local_token }), 401, "local_auth_required"],
    ];
    for (const [why, make, status, code] of rejected) {
      it(`rejects on /v1/responses: ${why}; authentication precedes policy, nothing upstream`, async () => {
        const planted = `${secret(2)} -----BEGIN PRIVATE KEY----- hunter2xyz`;
        await expectApiError(make().responses.create({ model: "qual-resp-json-ok", input: planted, store: false }), status, code, why);
        await assertNothingUpstream(assert, why);
        evidence.push({ gateway: inst.name, endpoint: "responses", check: why, status, code, upstream_connections: 0 });
      });
    }

    it("a valid local token without a provider Authorization is 401 missing_credential, nothing upstream", async () => {
      const r = await rawPost(inst.base, R, [`${LOCAL}: ${synthetic.local_token}`], respBody());
      assert.equal(r.status, 401);
      assert.equal(r.text, '{"error":{"code":"missing_credential"}}');
      await assertNothingUpstream(assert, "missing provider credential");
    });

    it("an authenticated but invalid Responses request (store missing) is refused by the protocol layer, nothing upstream", async () => {
      await expectApiError(withToken().responses.create({ model: "qual-resp-json-ok", input: "x" }), 422, "unsupported_input", "store missing");
      await assertNothingUpstream(assert, "store missing");
    });

    const malformed: Array<[string, string[]]> = [
      ["a duplicated header (both valid)", [`${LOCAL}: ${synthetic.local_token}`, `${LOCAL}: ${synthetic.local_token}`]],
      ["a Bearer prefix", [`${LOCAL}: Bearer ${synthetic.local_token}`]],
      ["a comma-joined list", [`${LOCAL}: ${synthetic.local_token},${synthetic.local_token_decoy}`]],
      ["an empty value", [`${LOCAL}:`]],
      ["31 bytes", [`${LOCAL}: ${"a".repeat(31)}`]],
      ["129 bytes", [`${LOCAL}: ${"a".repeat(129)}`]],
      ["characters outside the alphabet", [`${LOCAL}: ${synthetic.local_token.slice(0, 40)}+/=abc`]],
    ];
    for (const [why, lines] of malformed) {
      it(`malformed local header (${why}) on /v1/responses is 401 local_auth_invalid with the fixed body`, async () => {
        const r = await rawPost(inst.base, R, [provHeader, ...lines], respBody(`secret ${secret(3)}`));
        assert.equal(r.status, 401, why);
        assert.equal(r.text, '{"error":{"code":"local_auth_invalid"}}', why);
        await assertNothingUpstream(assert, why);
      });
    }

    it("a header nominated by Connection is removed on /v1/responses: 401 local_auth_required", async () => {
      const r = await rawPost(inst.base, R, [provHeader, `${LOCAL}: ${synthetic.local_token}`, "Connection: x-gateway-local-token"], respBody());
      assert.equal(r.status, 401);
      assert.equal(r.text, '{"error":{"code":"local_auth_required"}}');
      await assertNothingUpstream(assert, "hop-by-hop removal");
    });

    it("the header name is case-insensitive on /v1/responses and the accepted request still never forwards it", async () => {
      const r = await rawPost(inst.base, R, [provHeader, `x-gateway-local-token: ${synthetic.local_token}`], respBody());
      assert.equal(r.status, 200);
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      assert.equal(calls[0]!.local_token_seen, false);
    });

    it("wrong-route probes never reach the provider: Responses-shaped body on Chat and the reverse (authenticated)", async () => {
      const lines = [provHeader, `${LOCAL}: ${synthetic.local_token}`];
      assert.equal((await rawPost(inst.base, C, lines, respBody())).status, 422);
      assert.equal((await rawPost(inst.base, R, lines, chatBody())).status, 422);
      for (const target of ["/v1/responses/", "/v1/responses?x=1", "/V1/responses", "/v1/response"]) {
        const r = await rawPost(inst.base, target, lines, respBody());
        assert.ok(r.status >= 400 && r.status < 500, `${target}: ${r.status}`);
      }
      await assertNothingUpstream(assert, "wrong routes");
    });

    it("bounded unauthenticated load across both endpoints: every attempt is 401, nothing reaches the provider, both endpoints still serve", async () => {
      const total = 240;
      const wave = 40;
      const declared = 4 * 1024 * 1024; // an announced body that is never sent: it must never be waited for
      const statuses = new Map<string, number>();
      for (let i = 0; i < total; i += wave) {
        const batch = await Promise.all(
          Array.from({ length: wave }, (_, j) => {
            const k = i + j;
            const lines = k % 2 === 0 ? [provHeader] : [provHeader, `${LOCAL}: ${synthetic.local_token_decoy}`];
            return rawPost(inst.base, k % 4 < 2 ? R : C, lines, respBody(`secret ${secret(4)}`), declared);
          }),
        );
        for (const r of batch) statuses.set(`${r.status} ${r.text}`, (statuses.get(`${r.status} ${r.text}`) ?? 0) + 1);
      }
      assert.deepEqual([...statuses.keys()].sort(), ['401 {"error":{"code":"local_auth_invalid"}}', '401 {"error":{"code":"local_auth_required"}}']);
      assert.equal([...statuses.values()].reduce((a, b) => a + b, 0), total);
      await assertNothingUpstream(assert, "unauthenticated load");
      const c = withToken();
      assert.equal((await c.responses.create({ model: "qual-resp-json-ok", input: "after the load", store: false })).status, "completed");
      assert.equal((await c.chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: "after the load" }] })).choices[0]?.finish_reason, "stop");
      evidence.push({ gateway: inst.name, endpoint: "both", check: "unauthenticated load", attempts: total, upstream_connections: 0, still_serves: true });
    });
  });
}

describe("evidence (responses auth)", () => {
  it("records statuses and counts only, never a token value", () => {
    writeEvidence("responses-auth-node.json", { runtime: process.version, rows: evidence });
    assert.deepEqual(leaks(JSON.stringify(evidence)), []);
  });
});
