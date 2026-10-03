// An existing OpenAI SDK calling the Responses API through the local RedactSecret Gateway (Beta 1).
//
// What this does: one ordinary Responses call and one streamed call, both sent to the gateway's
// base URL instead of api.openai.com. The gateway inspects the request text with the RedactSecret
// core, forwards only the sanitized request to OpenAI, and relays the answer unchanged. Responses
// are NOT redacted: the provider's answer and its error bodies reach your application as sent.
// Request bytes that already reached the provider cannot be retracted if you abort or the gateway
// drops the connection.
//
// A Responses stream does not end with Chat's `finish_reason` or `[DONE]`. The provider ends it
// with one terminal event: `response.completed`, `response.failed` or `response.incomplete`. This
// example tells the four endings apart, because the end of the loop alone does not:
//   completed   success
//   failed      the PROVIDER declared failure (a normal, clean stream; the gateway relayed it)
//   incomplete  the PROVIDER declared the answer cut short (for example max_output_tokens)
//   none        no terminal event: the stream was truncated (the gateway or an intermediary cut it,
//               or the provider closed early). The gateway never invents a terminal event.
//
// You supply a real provider key at runtime in OPENAI_API_KEY. Never put a key in this file, in a
// config file, or in version control.
//
//   cd examples/node && npm ci --ignore-scripts
//   export OPENAI_API_KEY=...            # your own key, not stored anywhere by the gateway
//   npm run start:responses              # needs Node.js 24 or newer (runs TypeScript directly)
//   npm run start:responses -- --demo-redaction
//
// Environment: GATEWAY_BASE_URL (default http://127.0.0.1:8787/v1), EXAMPLE_MODEL (default
// gpt-4o-mini), and GATEWAY_LOCAL_TOKEN when the gateway enforces deployment.local_auth (sent in
// X-Gateway-Local-Token, a separate credential from OPENAI_API_KEY, never forwarded).
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

// maxRetries: 0 so a failed request is never silently sent to the provider a second time (see
// docs/contracts/errors-and-telemetry.md, "SDK retry guidance"). `store: false` is required by the
// gateway's stateless Responses contract.
const localToken = process.env.GATEWAY_LOCAL_TOKEN;
const client = new OpenAI({
  baseURL,
  apiKey,
  maxRetries: 0,
  ...(localToken ? { defaultHeaders: { "X-Gateway-Local-Token": localToken } } : {}),
});

// Synthetic and revoked-looking. It is not a credential; the gateway replaces it with a
// placeholder before anything leaves your machine.
const SYNTHETIC_TOKEN = "ghp_SYNTHETICREVOKED00000000000000000001";

const prompt = demoRedaction
  ? `My (synthetic) token is ${SYNTHETIC_TOKEN}. Reply with one short greeting.`
  : "Reply with one short greeting.";

async function main(): Promise<void> {
  const reply = await client.responses.create({ model, input: prompt, store: false });
  // An ordinary response carries its own status: a 200 can still say failed or incomplete.
  if (reply.status !== "completed") {
    throw new Error(`the provider reported status ${reply.status ?? "none"}: treat the output as unusable`);
  }
  console.log("response:", reply.output_text);

  const stream = await client.responses.create({ model, input: prompt, store: false, stream: true });
  let text = "";
  let terminal: "completed" | "failed" | "incomplete" | null = null;
  for await (const event of stream) {
    if (event.type === "response.output_text.delta") text += event.delta;
    if (event.type === "response.completed") terminal = "completed";
    if (event.type === "response.failed") terminal = "failed";
    if (event.type === "response.incomplete") terminal = "incomplete";
  }
  // On some Node.js versions fetch reports a cut stream as a normal end (observed on 22.16.0, not
  // on 24), so the loop ending proves nothing. Only the provider's terminal event does.
  if (terminal === null) {
    throw new Error("the stream ended without a terminal event: truncated, treat the output as partial");
  }
  if (terminal !== "completed") {
    throw new Error(`the provider declared the response ${terminal}: treat the output as unusable`);
  }
  console.log("stream:", text);
}

main().catch((err: unknown) => {
  if (err instanceof OpenAI.APIError) {
    console.error(`request failed: status ${err.status ?? "none"}, code ${err.code ?? "none"}`);
  } else {
    console.error("request failed:", err instanceof Error ? err.message : "unknown error");
  }
  process.exitCode = 1;
});
