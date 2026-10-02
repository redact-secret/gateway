// DRAFT: the proxy endpoint is not available until the Alpha 1 MVP (#8).
// Do not run this expecting success. Today the gateway skeleton answers
// POST /v1/chat/completions with local safe errors (422 `unsupported_input` for an unsupported body, 501 `not_implemented` for a valid one) and forwards nothing.
//
// Shows the intended shape only: an existing OpenAI SDK pointed at the gateway base URL.
// The key below is an obviously fake placeholder. Never put a real key in this file.
//
// Intended setup (not tested here): npm install openai@<pinned version> ; npx tsx this-file
import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "http://127.0.0.1:8787/v1", // the gateway, not the provider
  apiKey: "SYNTHETIC-PLACEHOLDER-NOT-A-REAL-KEY",
});

async function main(): Promise<void> {
  const reply = await client.chat.completions.create({
    model: "placeholder-model",
    messages: [{ role: "user", content: "hello" }],
  });
  console.log(reply.choices[0]?.message?.content);
}

main().catch((err: unknown) => {
  // Expected until #8: a 404 from the skeleton.
  console.error("draft example failed (expected until the Alpha 1 MVP):", err instanceof Error ? err.message : err);
  process.exitCode = 1;
});
