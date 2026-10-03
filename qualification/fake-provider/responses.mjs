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
//   qual-resp-err-<status>     provider error body (400 401 429 500 503)
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
export function makeResponsesScenario(h) {
  const { sendJson, beginSse, mark, tick, providerError } = h;

  return async function runResponsesScenario(call, req, res, json) {
    const streaming = json?.stream === true;
    let model = typeof json?.model === "string" ? json.model : "";
    if (model === "qual-resp-example" || model === "qual-example") model = streaming ? "qual-resp-sse-ok" : "qual-resp-json-ok";

    // The streamed scenarios script the stream only; their ordinary call (examples) is json-ok.
    if (!streaming && model.startsWith("qual-resp-sse-")) model = "qual-resp-json-ok";

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
    if (m) return sendJson(res, Number(m[1]), providerError(Number(m[1])));

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
            created(seq),
            ...textEvents,
            sse("response.output_text.done", { sequence_number: 80, item_id: "msg_qual", output_index: 0, content_index: 0, text: TEXT }),
            terminal(seq, "response.completed", "completed", { output: messageOutput() }),
          ]);
          return res.end();
        }
        case "qual-resp-sse-tool": {
          beginSse(res, call);
          const item = { id: "fc_qual", type: "function_call", call_id: "call_qual", name: "get_weather", arguments: ARGS.join(""), status: "completed" };
          await writeAll([
            created(seq),
            ...ARGS.map((a, i) => sse("response.function_call_arguments.delta", { sequence_number: i + 1, item_id: "fc_qual", output_index: 0, delta: a })),
            sse("response.function_call_arguments.done", { sequence_number: 80, item_id: "fc_qual", output_index: 0, arguments: ARGS.join("") }),
            terminal(seq, "response.completed", "completed", { output: [item] }),
          ]);
          return res.end();
        }
        case "qual-resp-sse-failed": {
          beginSse(res, call);
          await writeAll([created(seq), textEvents[0], terminal(seq, "response.failed", "failed", { error: { code: "server_error", message: "synthetic provider failure" } })]);
          return res.end();
        }
        case "qual-resp-sse-incomplete": {
          beginSse(res, call);
          await writeAll([created(seq), textEvents[0], terminal(seq, "response.incomplete", "incomplete", { incomplete_details: { reason: "max_output_tokens" } })]);
          return res.end();
        }
        case "qual-resp-sse-clean-no-terminal": {
          beginSse(res, call);
          await writeAll([created(seq), textEvents[0]]);
          return res.end();
        }
        case "qual-resp-sse-truncated": {
          beginSse(res, call);
          await writeAll([created(seq), textEvents[0]]);
          call.server_aborted = true;
          res.socket?.destroy();
          return;
        }
        case "qual-resp-sse-hang": {
          beginSse(res, call);
          await writeAll([created(seq), textEvents[0]]);
          return; // hold the stream open until the other side closes
        }
        default:
      }
    }
    return sendJson(res, 404, providerError(404));
  };
}
