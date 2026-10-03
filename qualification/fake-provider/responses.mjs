// Scripted FAKE provider scenarios for `POST /v1/responses` (#87). Synthetic data only.
//
// Loaded by server.mjs, which owns the HTTP server, the call log and the admin API. The scenario
// is chosen by the request `model`, exactly like the Chat scenarios, so it survives the gateway's
// sanitizing rewrite:
//
//   qual-resp-example          json-ok, or sse-ok when stream:true (the verified Responses examples)
//   qual-resp-json-ok          200, status "completed"
//   qual-resp-json-failed      200, status "failed" with an error object (provider-declared failure)
//   qual-resp-json-incomplete  200, status "incomplete" (provider-declared)
//   qual-resp-json-structured  200, completed, the output text is a JSON object (text.format / parse helpers; #88)
//   qual-resp-tool-call        200, completed, one function_call output item with provider-populated fields (#88)
//   qual-resp-tool-calls-parallel  the same with two function_call items (#88)
//   qual-resp-json-slow        replies after 150 ms (#88 cancellation and concurrency)
//   qual-resp-hang             never answers; ends when the caller (via the gateway) goes away (#88)
//   qual-resp-retry-429-then-ok  two 429s, then a completed response (#88 retry observation)
//   qual-resp-err-429-retry-after / qual-resp-err-500-hint-no-retry  retry hints the gateway must drop (#88)
//   qual-resp-err-<status>     provider error body (400 401 403 404 408 409 422 429 500 502 503 504)
//   qual-resp-sse-ok           created, text deltas, text done, response.completed, clean end
//   qual-resp-sse-tool         created, function-call argument deltas and done, response.completed
//   qual-resp-sse-failed       created, one delta, response.failed, clean end
//   qual-resp-sse-incomplete   created, one delta, response.incomplete, clean end
//   qual-resp-sse-clean-no-terminal  created, one delta, clean end: transport fine, no terminal event
//   qual-resp-sse-truncated    created, one delta, then the connection is destroyed (no terminating chunk)
//   qual-resp-sse-hang         created, one delta, then silence until the caller (or the gateway) closes
//
// The scenarios never emit anything the real provider would not (event names and payload shapes
// follow the documented Responses stream), and every text is synthetic.

const PIECES = ["Hel", "lo ", "안녕", "하세요", " 🙂", "!"];
const TEXT = PIECES.join("");
const ARGS = ['{"city":', '"서울"}'];

function sse(type, data) {
  return Buffer.from(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`, "utf8");
}

function responseObject(seq, status, extra = {}) {
  return { id: `resp_qual_${seq}`, object: "response", created_at: 1700000000, model: "qual", status, output: [], ...extra };
}

function created(seq) {
  return sse("response.created", { sequence_number: 0, response: responseObject(seq, "in_progress") });
}

// The events a real provider sends between `response.created` and the first delta. The SDKs'
// accumulating stream helpers (responses.stream) require them: without the output item and the
// content part a delta has nowhere to land ("missing output at index 0").
function opening(seq) {
  return [
    created(seq),
    sse("response.output_item.added", { sequence_number: 1, output_index: 0, item: { id: "msg_qual", type: "message", role: "assistant", status: "in_progress", content: [] } }),
    sse("response.content_part.added", { sequence_number: 2, item_id: "msg_qual", output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [] } }),
  ];
}

function delta(n, text) {
  return sse("response.output_text.delta", { sequence_number: n, item_id: "msg_qual", output_index: 0, content_index: 0, delta: text, logprobs: [] });
}

function terminal(seq, type, status, extra) {
  return sse(type, { sequence_number: 90, response: responseObject(seq, status, extra) });
}

function messageOutput() {
  return [{ id: "msg_qual", type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text: TEXT, annotations: [] }] }];
}

/** @param {{sendJson: Function, beginSse: Function, mark: Function, tick: Function, providerError: Function}} h */
const ERR_STATUSES = new Set([400, 401, 403, 404, 408, 409, 422, 429, 500, 502, 503, 504]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function toolItems(parallel) {
  const items = [
    { id: "fc_qual_1", type: "function_call", status: "completed", call_id: "call_qual_1", name: "get_weather", arguments: '{"city":"Seoul","days":3}' },
    { id: "fc_qual_2", type: "function_call", status: "completed", call_id: "call_qual_2", name: "get_weather", arguments: '{"city":"서울","days":1}' },
  ];
  return parallel ? items : items.slice(0, 1);
}

export function makeResponsesScenario(h) {
  const { sendJson, beginSse, mark, tick, providerError, scenarioCounts } = h;

  return async function runResponsesScenario(call, req, res, json) {
    const streaming = json?.stream === true;
    let model = typeof json?.model === "string" ? json.model : "";
    if (model === "qual-resp-example" || model === "qual-example") model = streaming ? "qual-resp-sse-ok" : "qual-resp-json-ok";

    // The streamed scenarios script the stream only; their ordinary call (examples) is json-ok.
    if (!streaming && model.startsWith("qual-resp-sse-")) model = "qual-resp-json-ok";

    const attempt = (scenarioCounts.get(model) ?? 0) + 1;
    scenarioCounts.set(model, attempt);
    const alive = () => !call.events.includes("closed");

    if (model === "qual-resp-tool-call" || model === "qual-resp-tool-calls-parallel") {
      return sendJson(res, 200, responseObject(call.seq, "completed", { output: toolItems(model === "qual-resp-tool-calls-parallel") }));
    }
    if (model === "qual-resp-json-structured") {
      const text = '{"city":"Seoul","days":3}';
      const output = [{ id: "msg_qual", type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text, annotations: [] }] }];
      return sendJson(res, 200, responseObject(call.seq, "completed", { output, output_text: text }));
    }
    if (model === "qual-resp-json-slow") {
      await sleep(150);
      if (!alive()) return;
      return sendJson(res, 200, responseObject(call.seq, "completed", { output: messageOutput(), output_text: TEXT }));
    }
    if (model === "qual-resp-hang") return; // never answer; ends when the client (via the gateway) goes away
    if (model === "qual-resp-retry-429-then-ok") {
      if (attempt <= 2) return sendJson(res, 429, providerError(429));
      return sendJson(res, 200, responseObject(call.seq, "completed", { output: messageOutput(), output_text: TEXT }));
    }
    if (model === "qual-resp-err-429-retry-after") return sendJson(res, 429, providerError(429), { "retry-after": "0" });
    if (model === "qual-resp-err-500-hint-no-retry") return sendJson(res, 500, providerError(500), { "x-should-retry": "false", "retry-after-ms": "10" });

    if (model === "qual-resp-json-ok") {
      return sendJson(res, 200, responseObject(call.seq, "completed", { output: messageOutput(), output_text: TEXT }));
    }
    if (model === "qual-resp-json-failed") {
      return sendJson(res, 200, responseObject(call.seq, "failed", { error: { code: "server_error", message: "synthetic provider failure" } }));
    }
    if (model === "qual-resp-json-incomplete") {
      return sendJson(res, 200, responseObject(call.seq, "incomplete", { incomplete_details: { reason: "max_output_tokens" } }));
    }
    const m = /^qual-resp-err-(\d{3})$/.exec(model);
    if (m && ERR_STATUSES.has(Number(m[1]))) return sendJson(res, Number(m[1]), providerError(Number(m[1])));

    if (streaming && model.startsWith("qual-resp-sse-")) {
      const seq = call.seq;
      const textEvents = PIECES.map((p, i) => delta(i + 1, p));
      const writeAll = async (events) => {
        for (const [i, e] of events.entries()) {
          res.write(e);
          if (i === 0) mark(call, "first_event");
          await tick();
        }
      };
      switch (model) {
        case "qual-resp-sse-ok": {
          beginSse(res, call);
          await writeAll([
            ...opening(seq),
            ...textEvents,
            sse("response.output_text.done", { sequence_number: 80, item_id: "msg_qual", output_index: 0, content_index: 0, text: TEXT }),
            sse("response.content_part.done", { sequence_number: 81, item_id: "msg_qual", output_index: 0, content_index: 0, part: { type: "output_text", text: TEXT, annotations: [] } }),
            sse("response.output_item.done", { sequence_number: 82, output_index: 0, item: messageOutput()[0] }),
            terminal(seq, "response.completed", "completed", { output: messageOutput() }),
          ]);
          return res.end();
        }
        case "qual-resp-sse-tool": {
          beginSse(res, call);
          const item = { id: "fc_qual", type: "function_call", call_id: "call_qual", name: "get_weather", arguments: ARGS.join(""), status: "completed" };
          await writeAll([
            created(seq),
            sse("response.output_item.added", { sequence_number: 1, output_index: 0, item: { id: "fc_qual", type: "function_call", call_id: "call_qual", name: "get_weather", arguments: "", status: "in_progress" } }),
            ...ARGS.map((a, i) => sse("response.function_call_arguments.delta", { sequence_number: i + 1, item_id: "fc_qual", output_index: 0, delta: a })),
            sse("response.function_call_arguments.done", { sequence_number: 80, item_id: "fc_qual", output_index: 0, arguments: ARGS.join("") }),
            sse("response.output_item.done", { sequence_number: 81, output_index: 0, item }),
            terminal(seq, "response.completed", "completed", { output: [item] }),
          ]);
          return res.end();
        }
        case "qual-resp-sse-failed": {
          beginSse(res, call);
          await writeAll([...opening(seq), textEvents[0], terminal(seq, "response.failed", "failed", { error: { code: "server_error", message: "synthetic provider failure" } })]);
          return res.end();
        }
        case "qual-resp-sse-incomplete": {
          beginSse(res, call);
          await writeAll([...opening(seq), textEvents[0], terminal(seq, "response.incomplete", "incomplete", { incomplete_details: { reason: "max_output_tokens" } })]);
          return res.end();
        }
        case "qual-resp-sse-clean-no-terminal": {
          beginSse(res, call);
          await writeAll([...opening(seq), textEvents[0]]);
          return res.end();
        }
        case "qual-resp-sse-truncated": {
          beginSse(res, call);
          await writeAll([...opening(seq), textEvents[0]]);
          call.server_aborted = true;
          res.socket?.destroy();
          return;
        }
        case "qual-resp-sse-hang": {
          beginSse(res, call);
          await writeAll([...opening(seq), textEvents[0]]);
          return; // hold the stream open until the other side closes
        }
        default:
      }
    }
    return sendJson(res, 404, providerError(404));
  };
}
