// Responses (POST /v1/responses) qualification helpers (#88), Node. Synthetic only.
//
// The case table (qualification/responses-cases.json) is shared with the Python harness and has the
// shape of the Alpha 2 table: SDK request parameters with synthetic secret tokens and an
// expectation. The expected sanitized body is derived from the SAME parameters by replacing each
// secret token with the placeholder the contract numbers it to, so a mismatch in any retained
// structure (items, roles, call ids, argument trees, schema keywords, tool_choice, text format,
// metadata, controls) is a failure, not just a missing 200.
import { readFileSync } from "node:fs";
import path from "node:path";
import { mapDeep, substitute, type Json, type RawResponse } from "./alpha2-support.ts";
import { gateways, synthetic } from "./support.ts";

export interface RCase {
  id: string;
  group: string;
  gateways?: string[];
  slots: string[];
  endpoint?: "chat" | "responses";
  params?: Record<string, Json>;
  raw?: string;
  raw_hex?: string;
  expect: { kind: "forward" | "reject"; status?: number; code?: string; map?: Record<string, number> };
  exact_arguments?: string[];
  plaintext_forwarded?: boolean;
  stream?: boolean;
}

const file = path.join(path.dirname(process.env["QUAL_SYNTHETIC"] ?? ""), "responses-cases.json");
export const rcases: RCase[] = (JSON.parse(readFileSync(file, "utf8")) as { cases: RCase[] }).cases;

export const rgatewayFor: Record<string, string> = {
  standard: gateways.standard,
  forward: gateways.policyForward,
  common: gateways.policyCommon,
  tight: gateways.tight,
  concurrent: gateways.concurrent,
};

export function plainParams(c: RCase): Record<string, Json> {
  return mapDeep(c.params as Json, (s) => substitute(s, null)) as Record<string, Json>;
}

export function expectedBody(c: RCase): Json {
  const map = c.expect.map ?? {};
  if (c.params === undefined) return JSON.parse(substitute(c.raw ?? "", map)) as Json; // raw rows: the escapes decode first
  return mapDeep(c.params as Json, (s) => substitute(s, map));
}

export function rawBytes(c: RCase): Buffer {
  if (c.raw_hex !== undefined) return Buffer.from(c.raw_hex, "hex");
  return Buffer.from(substitute(c.raw ?? "", null), "utf8");
}

/** Parse every function_call `arguments` string so trees compare structurally; list the raw strings. */
export function normalize(body: Json): { normalized: Json; argumentStrings: string[] } {
  const argumentStrings: string[] = [];
  const copy = JSON.parse(JSON.stringify(body)) as { input?: unknown };
  if (Array.isArray(copy.input)) {
    for (const item of copy.input as Array<Record<string, unknown>>) {
      if (item !== null && typeof item === "object" && item["type"] === "function_call" && typeof item["arguments"] === "string") {
        argumentStrings.push(item["arguments"]);
        try {
          item["arguments"] = JSON.parse(item["arguments"]);
        } catch {
          /* left as the string; the comparison then fails loudly */
        }
      }
    }
  }
  return { normalized: copy as unknown as Json, argumentStrings };
}

export interface RawResult extends RawResponse {}

/** A hand-built POST (no SDK) to the named endpoint, for bodies an SDK cannot produce. */
export async function rawPostTo(base: string, endpoint: "chat" | "responses", bytes: Buffer, apiKey: string, extraHeaders: Record<string, string> = {}): Promise<RawResult> {
  const target = endpoint === "chat" ? "/v1/chat/completions" : "/v1/responses";
  const res = await fetch(`${base}${target}`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}`, ...extraHeaders },
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

export { synthetic };
