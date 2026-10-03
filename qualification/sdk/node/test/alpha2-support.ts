// Alpha 2 qualification helpers (#57), Node. Synthetic only.
//
// The case table (qualification/alpha2-cases.json) is shared with the Python harness: each case
// carries SDK request parameters with synthetic secret tokens and an expectation. The expected
// sanitized body is derived from the SAME parameters by replacing each secret token with the
// placeholder the contract numbers it to, so a mismatch in any retained structure (roles, ids,
// argument trees, schema keywords, tool_choice, metadata) is a failure, not just a missing 200.
import { readFileSync } from "node:fs";
import path from "node:path";
import { gateways, provider, secret, type Call } from "./support.ts";

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

export interface Expect {
  kind: "forward" | "reject";
  status?: number;
  code?: string;
  map?: Record<string, number>;
  body?: Json;
}

export interface Case {
  id: string;
  group: string;
  gateways?: string[];
  params?: Record<string, Json>;
  raw?: string;
  raw_hex?: string;
  expect: Expect;
  note?: string;
  exact_arguments?: string[];
  plaintext_forwarded?: boolean;
}

const casesFile = path.join(path.dirname(process.env["QUAL_SYNTHETIC"] ?? ""), "alpha2-cases.json");
const loaded = JSON.parse(readFileSync(casesFile, "utf8")) as { tokens: Record<string, string>; cases: Case[] };
export const cases: Case[] = loaded.cases;
const tokens = loaded.tokens;

export const gatewayFor: Record<string, string> = {
  standard: gateways.standard,
  forward: gateways.policyForward,
  common: gateways.policyCommon,
  tight: gateways.tight,
  concurrent: gateways.concurrent,
};

export function escapeAll(text: string): string {
  return [...text].map((ch) => `\\u${ch.charCodeAt(0).toString(16).padStart(4, "0")}`).join("");
}

/** Replace tokens in a string. `placeholders` null: every token becomes its plaintext. */
export function substitute(text: string, placeholders: Record<string, number> | null): string {
  return text.replace(/\{\{(E?)(S\d+|PEM|WARN)\}\}/g, (_m, esc: string, name: string) => {
    if (name === "PEM" || name === "WARN") return tokens[name] ?? "";
    const n = placeholders?.[name];
    if (n !== undefined) return `<SECRET_${n}>`;
    const plain = secret(Number(name.slice(1)));
    return esc ? escapeAll(plain) : plain;
  });
}

/** Map every string value AND object key. */
export function mapDeep(value: Json, f: (s: string) => string): Json {
  if (typeof value === "string") return f(value);
  if (Array.isArray(value)) return value.map((v) => mapDeep(v, f));
  if (value !== null && typeof value === "object") {
    const out: { [k: string]: Json } = {};
    for (const [k, v] of Object.entries(value)) out[f(k)] = mapDeep(v, f);
    return out;
  }
  return value;
}

/** Parse every tool-call `arguments` string so trees compare structurally; list the raw strings. */
export function normalize(body: Json): { normalized: Json; argumentStrings: string[] } {
  const argumentStrings: string[] = [];
  const copy = JSON.parse(JSON.stringify(body)) as { messages?: Array<{ tool_calls?: Array<{ function: { arguments: string } }> }> };
  for (const m of copy.messages ?? []) {
    for (const c of m.tool_calls ?? []) {
      argumentStrings.push(c.function.arguments);
      try {
        (c.function as { arguments: unknown }).arguments = JSON.parse(c.function.arguments);
      } catch {
        /* left as the string; the comparison then fails loudly */
      }
    }
  }
  return { normalized: copy as unknown as Json, argumentStrings };
}

export function plainParams(c: Case): Record<string, Json> {
  return mapDeep(c.params as Json, (s) => substitute(s, null)) as Record<string, Json>;
}

export function expectedBody(c: Case): Json {
  if (c.expect.body !== undefined) return c.expect.body;
  const map = c.expect.map ?? {};
  return mapDeep(c.params as Json, (s) => substitute(s, map));
}

export function rawBytes(c: Case): Buffer {
  if (c.raw_hex !== undefined) return Buffer.from(c.raw_hex, "hex");
  return Buffer.from(substitute(c.raw ?? "", null), "utf8");
}

export interface RawResponse {
  status: number;
  code: string | null;
  text: string;
}

/** A hand-built POST (no SDK), for bodies an SDK cannot produce: duplicate keys, bad UTF-8. */
export async function rawPost(base: string, bytes: Buffer, apiKey: string): Promise<RawResponse> {
  const res = await fetch(`${base}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}` },
    body: new Uint8Array(bytes),
  });
  const text = await res.text();
  let code: string | null = null;
  try {
    code = (JSON.parse(text) as { error?: { code?: string } }).error?.code ?? null;
  } catch {
    /* not JSON */
  }
  return { status: res.status, code, text };
}

export function plantedIn(text: string): string[] {
  return ["ghp_SYNTH", "BEGIN PRIVATE KEY", "U1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ"].filter((m) => text.includes(m));
}

export function placeholderCount(value: Json): number {
  return JSON.stringify(value).split("<SECRET_").length - 1;
}

export function sentBodies(calls: Call[]): string[] {
  return calls.map((c) => c.body);
}

export { provider };
