// An existing OpenAI SDK pointed at the local RedactSecret Gateway (Alpha 1).
//
// What this does: one ordinary Chat Completions call and one streamed call, both sent to the
// gateway's base URL instead of api.openai.com. The gateway inspects the prompt text with the
// RedactSecret core, forwards only the sanitized request to OpenAI, and relays the answer
// unchanged. Responses are NOT redacted.
//
// You supply a real provider key at runtime in OPENAI_API_KEY. It is sent to the gateway as
// the usual Bearer token and forwarded only to the provider. Never put a key in this file,
// in a config file, or in version control.
//
//   cd examples/node && npm ci --ignore-scripts
//   export OPENAI_API_KEY=...            # your own key, not stored anywhere by the gateway
//   npm start                            # needs Node.js 24 or newer (runs TypeScript directly)
//   npm start -- --demo-redaction        # also sends a synthetic, revoked-looking token
//
// Environment: GATEWAY_BASE_URL (default http://127.0.0.1:8787/v1), EXAMPLE_MODEL (default
// gpt-4o-mini; use a chat model your account can call).
//
// Verified how: this file is type-checked and run in CI against the NON-RELEASE qualification
// build with a scripted fake provider and a synthetic key (.github/workflows/qualification.yml).
// It has never been run against the real provider by this repository.
import OpenAI from "openai";

const apiKey = process.env.OPENAI_API_KEY;
if (!apiKey) {
  console.error("Set OPENAI_API_KEY to your own provider key (never commit it).");
  process.exit(2);
}
const baseURL = process.env.GATEWAY_BASE_URL ?? "http://127.0.0.1:8787/v1";
const model = process.env.EXAMPLE_MODEL ?? "gpt-4o-mini";
const demoRedaction = process.argv.includes("--demo-redaction");

// maxRetries: 0 so a failed request is never silently sent to the provider a second time. The
// SDK default is 2 retries for 408/409/429/5xx and connection errors, and a retry of a request
// that already reached the provider can duplicate (and bill) provider-side work. Raise it only
// if you accept that. See docs/contracts/errors-and-telemetry.md ("SDK retry guidance").
const client = new OpenAI({ baseURL, apiKey, maxRetries: 0 });

// Synthetic and revoked-looking. It is not a credential; the gateway replaces it with a
// placeholder before anything leaves your machine.
const SYNTHETIC_TOKEN = "ghp_SYNTHETICREVOKED00000000000000000001";

const prompt = demoRedaction
  ? `My (synthetic) token is ${SYNTHETIC_TOKEN}. Reply with one short greeting.`
  : "Reply with one short greeting.";

async function main(): Promise<void> {
  const reply = await client.chat.completions.create({
    model,
    messages: [{ role: "user", content: prompt }],
  });
  console.log("completion:", reply.choices[0]?.message.content);

  const stream = await client.chat.completions.create({
    model,
    messages: [{ role: "user", content: prompt }],
    stream: true,
  });
  let text = "";
  let finishReason: string | null = null;
  for await (const chunk of stream) {
    text += chunk.choices[0]?.delta?.content ?? "";
    finishReason = chunk.choices[0]?.finish_reason ?? finishReason;
  }
  // The gateway ends a broken stream abruptly and never invents a completion. Depending on the
  // Node.js version, fetch may report that as a normal end (observed on 22.16.0, not on 24), so
  // the version-independent completeness check is the completion indicator the provider sends.
  if (finishReason === null) {
    throw new Error("the stream ended without a completion event: treat the output as truncated");
  }
  console.log("stream:", text);
}

main().catch((err: unknown) => {
  if (err instanceof OpenAI.APIError) {
    // The gateway's own errors carry a fixed safe code (for example unsupported_input).
    console.error(`request failed: status ${err.status ?? "none"}, code ${err.code ?? "none"}`);
  } else {
    console.error("request failed:", err instanceof Error ? err.message : "unknown error");
  }
  process.exitCode = 1;
});
