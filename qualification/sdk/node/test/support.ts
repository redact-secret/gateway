// Shared helpers for the Node.js/TypeScript SDK qualification (ADR 0020). Synthetic only.
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import OpenAI from "openai";

function env(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`missing environment variable ${name} (run via qualification/run-suites.sh)`);
  return v;
}

export const synthetic = JSON.parse(readFileSync(env("QUAL_SYNTHETIC"), "utf8")) as {
  api_key: string;
  local_token: string;
  local_token_decoy: string;
  secret_prefix: string;
  prompt_marker_prefix: string;
  stream_text: string;
};

export const gateways = {
  standard: env("GATEWAY_STANDARD"),
  tight: env("GATEWAY_TIGHT"),
  noUpstream: env("GATEWAY_NOUPSTREAM"),
  deadProvider: env("GATEWAY_DEADPROVIDER"),
  overload: env("GATEWAY_OVERLOAD"),
  policyForward: env("GATEWAY_POLICYFORWARD"),
  policyCommon: env("GATEWAY_POLICYCOMMON"),
  concurrent: env("GATEWAY_CONCURRENT"),
  authFile: env("GATEWAY_AUTHFILE"),
  authEnv: env("GATEWAY_AUTHENV"),
};

const admin = env("QUAL_ADMIN");
/** Direct (gateway-bypassing) provider address, for controls only. */
export const directProvider = env("QUAL_PROVIDER");

export interface Call {
  seq: number;
  method: string;
  path: string;
  header_names: string[];
  has_authorization: boolean;
  authorization_sha256: string | null;
  content_type: string | null;
  user_agent: string | null;
  local_token_seen: boolean;
  body: string;
  body_bytes: number;
  scenario: string;
  events: string[];
  times: Record<string, number>;
  server_aborted: boolean;
}

async function adminJson<T>(pathAndQuery: string, method = "GET"): Promise<T> {
  const res = await fetch(`${admin}${pathAndQuery}`, { method });
  return (await res.json()) as T;
}

export const provider = {
  async reset(): Promise<void> {
    await adminJson("/__admin/reset", "POST");
  },
  async calls(): Promise<{ connections: number; calls: Call[] }> {
    return adminJson("/__admin/calls");
  },
  /** Event-driven wait for a recorded call event; false only when the deadline passes. */
  async awaitEvent(seq: number, event: string, timeoutMs = 10_000): Promise<boolean> {
    const r = await adminJson<{ ok: boolean }>(
      `/__admin/await?call=${seq}&event=${event}&timeout_ms=${timeoutMs}`,
    );
    return r.ok;
  },
  async awaitCalls(count: number, timeoutMs = 10_000): Promise<boolean> {
    const r = await adminJson<{ ok: boolean }>(`/__admin/await-calls?count=${count}&timeout_ms=${timeoutMs}`);
    return r.ok;
  },
  async release(seq: number): Promise<void> {
    await adminJson(`/__admin/release?call=${seq}`, "POST");
  },
};

export function client(base: string, options: ConstructorParameters<typeof OpenAI>[0] = {}): OpenAI {
  return new OpenAI({ baseURL: `${base}/v1`, apiKey: synthetic.api_key, maxRetries: 0, ...options });
}

/** A synthetic token the pinned core detects (`full` profile). Never a real credential. */
export function secret(n: number): string {
  return `${synthetic.secret_prefix}${String(n).padStart(20, "0")}`;
}

export function promptMarker(): string {
  return `${synthetic.prompt_marker_prefix}${randomUUID()}`;
}

export const authSha = (): string =>
  createHash("sha256").update(`Bearer ${synthetic.api_key}`).digest("hex");

/** Nothing reached the provider: no connection and no request. */
export async function assertNothingUpstream(
  assert: { equal: (a: unknown, b: unknown, m?: string) => void },
  why: string,
): Promise<void> {
  const { connections, calls } = await provider.calls();
  assert.equal(connections, 0, `${why}: provider saw a connection`);
  assert.equal(calls.length, 0, `${why}: provider saw a request`);
}

export function replaceDeep(value: unknown, from: string, to: string): unknown {
  return JSON.parse(JSON.stringify(value).split(from).join(to));
}

export function writeEvidence(name: string, data: unknown): void {
  const dir = env("QUAL_EVIDENCE");
  mkdirSync(dir, { recursive: true });
  writeFileSync(path.join(dir, name), `${JSON.stringify(data, null, 2)}\n`);
}

/** A loopback port with nothing listening (connection refused). */
export async function closedPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.listen(0, "127.0.0.1", () => {
      const port = (s.address() as net.AddressInfo).port;
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}

export function leaks(text: string): string[] {
  return [synthetic.local_token, synthetic.local_token_decoy, synthetic.secret_prefix, "ghp_SYNTH", synthetic.prompt_marker_prefix, synthetic.api_key, "hunter2xyz", "U1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ"].filter((m) =>
    text.includes(m),
  );
}
