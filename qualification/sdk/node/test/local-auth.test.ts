// Caller and provider credentials are separate authorities (#65, docs/contracts/local-caller-auth.md),
// seen through the pinned Node SDK, raw HTTP for the shapes an SDK cannot emit, and the fake
// provider's own record as the oracle: the provider must never receive the local token, and a
// rejection delivers no upstream connection and no request body.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import net from "node:net";
import { beforeEach, describe, it } from "node:test";
import OpenAI from "openai";
import { assertNothingUpstream, authSha, client, gateways, leaks, provider, secret, synthetic, writeEvidence } from "./support.ts";

const LOCAL = "X-Gateway-Local-Token";
const evidence: Array<Record<string, unknown>> = [];

// Two instances with different static content policy and different token delivery.
const instances = [
  { name: "authfile", base: gateways.authFile, delivery: "file", redacts: true },
  { name: "authenv", base: gateways.authEnv, delivery: "env", redacts: false },
] as const;

function body(content = "hello"): string {
  return JSON.stringify({ model: "qual-json-ok", messages: [{ role: "user", content }] });
}

/** One raw HTTP/1.1 POST; resolves with the status and body even if the server resets after replying. */
function rawPost(
  base: string,
  headerLines: string[],
  payload: string,
  declaredLength: number = Buffer.byteLength(payload),
): Promise<{ status: number; text: string }> {
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
      const head = [
        "POST /v1/chat/completions HTTP/1.1",
        `Host: ${hostname}:${port}`,
        "Content-Type: application/json",
        `Content-Length: ${declaredLength}`,
        "Connection: close",
        ...headerLines,
        "",
        "",
      ].join("\r\n");
      socket.write(head + payload);
    });
  });
}

const provHeader = `Authorization: Bearer ${synthetic.api_key}`;

async function expectApiError(p: Promise<unknown>, status: number, code: string, why: string): Promise<InstanceType<typeof OpenAI.APIError>> {
  let caught: unknown;
  try {
    await p;
  } catch (e) {
    caught = e;
  }
  assert.ok(caught instanceof OpenAI.APIError, `${why}: expected an API error`);
  assert.equal(caught.status, status, `${why}: status`);
  assert.equal(caught.code, code, `${why}: safe code`);
  const rendered = JSON.stringify({ m: caught.message, e: caught.error, h: [...(caught.headers?.entries?.() ?? [])] });
  assert.deepEqual(leaks(rendered), [], `${why}: SDK-visible error exposes a secret value`);
  return caught;
}

for (const inst of instances) {
  describe(`local caller authentication on ${inst.name} (${inst.delivery} token, ${inst.redacts ? "full profile" : "common profile, on_warn forward"})`, () => {
    beforeEach(provider.reset);

    it("both credentials: the provider gets the provider key and no trace of the local token", async () => {
      const c = client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token } });
      const reply = await c.chat.completions.create({
        model: "qual-json-ok",
        messages: [{ role: "user", content: `token ${secret(1)}` }],
      });
      assert.equal(reply.choices[0]?.finish_reason, "stop");
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      const call = calls[0]!;
      assert.equal(call.authorization_sha256, authSha(), "provider key forwarded unchanged");
      assert.ok(!call.header_names.includes(LOCAL.toLowerCase()), "local header must not be forwarded");
      assert.equal(call.local_token_seen, false, "local token (or decoy) appeared in a header, the target or the body");
      assert.ok(!call.body.includes(synthetic.local_token));
      if (inst.redacts) assert.ok(call.body.includes("<SECRET_1>") && !call.body.includes("ghp_"));
      evidence.push({ gateway: inst.name, check: "both credentials", status: 200, upstream_calls: 1, local_token_seen: false });
    });

    it("a streamed request carries the same separation", async () => {
      const c = client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token } });
      const stream = await c.chat.completions.create({
        model: "qual-example",
        messages: [{ role: "user", content: "hello" }],
        stream: true,
      });
      let finish: string | null = null;
      for await (const chunk of stream) finish = chunk.choices[0]?.finish_reason ?? finish;
      assert.equal(finish, "stop");
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      assert.equal(calls[0]!.local_token_seen, false);
      assert.ok(!calls[0]!.header_names.includes(LOCAL.toLowerCase()));
    });

    const rejected: Array<[string, () => OpenAI, number, string]> = [
      ["no local token", () => client(inst.base), 401, "local_auth_required"],
      [
        "the provider key sent as the local token",
        () => client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.api_key } }),
        401,
        "local_auth_invalid",
      ],
      [
        "a different well-formed token",
        () => client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token_decoy } }),
        401,
        "local_auth_invalid",
      ],
      [
        "the local token sent only as the provider key (Authorization is never read for local auth)",
        () => client(inst.base, { apiKey: synthetic.local_token }),
        401,
        "local_auth_required",
      ],
    ];
    for (const [why, make, status, code] of rejected) {
      it(`rejects: ${why}`, async () => {
        const planted = `${secret(2)} -----BEGIN PRIVATE KEY----- hunter2xyz`;
        // Content that would be a policy outcome (block or redact) is never reached: auth is first.
        const err = await expectApiError(
          make().chat.completions.create({ model: "qual-json-ok", messages: [{ role: "user", content: planted }] }),
          status,
          code,
          why,
        );
        await assertNothingUpstream(assert, why);
        evidence.push({ gateway: inst.name, check: why, status: err.status, code: err.code, upstream_connections: 0 });
      });
    }

    it("a valid local token without a provider Authorization is 401 missing_credential, nothing upstream", async () => {
      const r = await rawPost(inst.base, [`${LOCAL}: ${synthetic.local_token}`], body());
      assert.equal(r.status, 401);
      assert.equal(r.text, '{"error":{"code":"missing_credential"}}');
      await assertNothingUpstream(assert, "missing provider credential");
    });

    const malformed: Array<[string, string[]]> = [
      ["a duplicated header (both valid)", [`${LOCAL}: ${synthetic.local_token}`, `${LOCAL}: ${synthetic.local_token}`]],
      ["a Bearer prefix", [`${LOCAL}: Bearer ${synthetic.local_token}`]],
      ["a comma-joined list", [`${LOCAL}: ${synthetic.local_token},${synthetic.local_token_decoy}`]],
      ["an embedded space", [`${LOCAL}: ${synthetic.local_token.slice(0, 20)} ${synthetic.local_token.slice(20)}`]],
      ["an empty value", [`${LOCAL}:`]],
      ["31 bytes", [`${LOCAL}: ${"a".repeat(31)}`]],
      ["129 bytes", [`${LOCAL}: ${"a".repeat(129)}`]],
      ["a quoted value", [`${LOCAL}: "${synthetic.local_token}"`]],
      ["characters outside the alphabet", [`${LOCAL}: ${synthetic.local_token.slice(0, 40)}+/=abc`]],
    ];
    for (const [why, lines] of malformed) {
      it(`malformed local header (${why}) is 401 local_auth_invalid with the fixed body`, async () => {
        const r = await rawPost(inst.base, [provHeader, ...lines], body(`secret ${secret(3)}`));
        assert.equal(r.status, 401, why);
        assert.equal(r.text, '{"error":{"code":"local_auth_invalid"}}', why);
        assert.deepEqual(leaks(r.text), []);
        await assertNothingUpstream(assert, why);
      });
    }

    it("a header nominated by Connection is removed: 401 local_auth_required", async () => {
      const r = await rawPost(inst.base, [provHeader, `${LOCAL}: ${synthetic.local_token}`, "Connection: x-gateway-local-token"], body());
      assert.equal(r.status, 401);
      assert.equal(r.text, '{"error":{"code":"local_auth_required"}}');
      await assertNothingUpstream(assert, "hop-by-hop removal");
    });

    it("the header name is case-insensitive and the accepted request still never forwards it", async () => {
      const r = await rawPost(inst.base, [provHeader, `x-gateway-local-token: ${synthetic.local_token}`], body());
      assert.equal(r.status, 200);
      const { calls } = await provider.calls();
      assert.equal(calls.length, 1);
      assert.equal(calls[0]!.local_token_seen, false);
    });

    it("health and readiness never need the token and ignore a wrong one", async () => {
      for (const route of ["/healthz", "/readyz"]) {
        for (const headers of [{}, { [LOCAL]: synthetic.local_token_decoy }] as Array<Record<string, string>>) {
          const res = await fetch(`${inst.base}${route}`, { headers });
          assert.equal(res.status, 200, route);
          assert.deepEqual(leaks(await res.text()), []);
        }
      }
      await assertNothingUpstream(assert, "health");
    });

    it("bounded unauthenticated load: every attempt is 401, nothing reaches the provider, the gateway still serves", async () => {
      const total = 240;
      const wave = 40;
      const declared = 4 * 1024 * 1024; // an announced body that is never sent: it must never be waited for
      const statuses = new Map<string, number>();
      for (let i = 0; i < total; i += wave) {
        const batch = await Promise.all(
          Array.from({ length: wave }, (_, j) => {
            const lines = (i + j) % 2 === 0 ? [provHeader] : [provHeader, `${LOCAL}: ${synthetic.local_token_decoy}`];
            return rawPost(inst.base, lines, body(`secret ${secret(4)}`), declared);
          }),
        );
        for (const r of batch) statuses.set(`${r.status} ${r.text}`, (statuses.get(`${r.status} ${r.text}`) ?? 0) + 1);
      }
      assert.deepEqual([...statuses.keys()].sort(), [
        '401 {"error":{"code":"local_auth_invalid"}}',
        '401 {"error":{"code":"local_auth_required"}}',
      ]);
      assert.equal([...statuses.values()].reduce((a, b) => a + b, 0), total);
      await assertNothingUpstream(assert, "unauthenticated load");
      const ok = await client(inst.base, { defaultHeaders: { [LOCAL]: synthetic.local_token } }).chat.completions.create({
        model: "qual-json-ok",
        messages: [{ role: "user", content: "after the load" }],
      });
      assert.equal(ok.choices[0]?.finish_reason, "stop");
      evidence.push({ gateway: inst.name, check: "unauthenticated load", attempts: total, upstream_connections: 0, still_serves: true });
    });
  });
}

describe("evidence", () => {
  it("records the sha-256 of the provider Authorization only, never a token value", () => {
    const sha = createHash("sha256").update(`Bearer ${synthetic.api_key}`).digest("hex");
    assert.equal(sha, authSha());
    writeEvidence("local-auth-node.json", { runtime: process.version, rows: evidence });
    assert.deepEqual(leaks(JSON.stringify(evidence)), []);
  });
});
