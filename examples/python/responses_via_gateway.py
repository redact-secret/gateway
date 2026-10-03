"""An existing OpenAI SDK calling the Responses API through the local RedactSecret Gateway (Beta 1).

What this does: one ordinary Responses call and one streamed call, both sent to the gateway's base
URL instead of api.openai.com. The gateway inspects the request text with the RedactSecret core,
forwards only the sanitized request to OpenAI, and relays the answer unchanged. Responses are NOT
redacted: the provider's answer and its error bodies reach your application as sent. Request bytes
that already reached the provider cannot be retracted if you abort or the gateway drops the
connection.

A Responses stream does not end with Chat's `finish_reason` or `[DONE]`. The provider ends it with
one terminal event: `response.completed`, `response.failed` or `response.incomplete`. This example
tells the four endings apart, because the end of the loop alone does not:
  completed   success
  failed      the PROVIDER declared failure (a normal, clean stream; the gateway relayed it)
  incomplete  the PROVIDER declared the answer cut short (for example max_output_tokens)
  none        no terminal event: the stream was truncated (the gateway or an intermediary cut it,
              or the provider closed early). The gateway never invents a terminal event.

You supply a real provider key at runtime in OPENAI_API_KEY. Never put a key in this file, in a
config file, or in version control.

    cd examples/python
    python3 -m venv .venv
    .venv/bin/pip install --require-hashes --no-deps -r requirements.txt
    export OPENAI_API_KEY=...            # your own key, not stored anywhere by the gateway
    .venv/bin/python responses_via_gateway.py
    .venv/bin/python responses_via_gateway.py --demo-redaction   # also sends a synthetic token

Environment: GATEWAY_BASE_URL (default http://127.0.0.1:8787/v1), EXAMPLE_MODEL (default
gpt-4o-mini), and GATEWAY_LOCAL_TOKEN when the gateway enforces deployment.local_auth (sent in
X-Gateway-Local-Token, a separate credential from OPENAI_API_KEY, never forwarded).

Verified how: this file is run in CI against the NON-RELEASE qualification build with a scripted
fake provider and a synthetic key (.github/workflows/qualification.yml). It has never been run
against the real provider by this repository.
"""

import os
import sys

import openai
from openai import OpenAI

api_key = os.environ.get("OPENAI_API_KEY")
if not api_key:
    print("Set OPENAI_API_KEY to your own provider key (never commit it).", file=sys.stderr)
    raise SystemExit(2)

base_url = os.environ.get("GATEWAY_BASE_URL", "http://127.0.0.1:8787/v1")
model = os.environ.get("EXAMPLE_MODEL", "gpt-4o-mini")
demo_redaction = "--demo-redaction" in sys.argv[1:]

# max_retries=0 so a failed request is never silently sent to the provider a second time (see
# docs/contracts/errors-and-telemetry.md, "SDK retry guidance"). `store=False` is required by the
# gateway's stateless Responses contract.
local_token = os.environ.get("GATEWAY_LOCAL_TOKEN")
client = OpenAI(
    base_url=base_url,
    api_key=api_key,
    max_retries=0,
    default_headers={"X-Gateway-Local-Token": local_token} if local_token else None,
)

# Synthetic and revoked-looking. It is not a credential; the gateway replaces it with a
# placeholder before anything leaves your machine.
SYNTHETIC_TOKEN = "ghp_SYNTHETICREVOKED00000000000000000001"

prompt = (
    f"My (synthetic) token is {SYNTHETIC_TOKEN}. Reply with one short greeting."
    if demo_redaction
    else "Reply with one short greeting."
)

TERMINALS = {"response.completed": "completed", "response.failed": "failed", "response.incomplete": "incomplete"}


def main() -> None:
    reply = client.responses.create(model=model, input=prompt, store=False)
    # An ordinary response carries its own status: a 200 can still say failed or incomplete.
    if reply.status != "completed":
        raise RuntimeError(f"the provider reported status {reply.status}: treat the output as unusable")
    print("response:", reply.output_text)

    text, terminal = "", None
    with client.responses.create(model=model, input=prompt, store=False, stream=True) as stream:
        # A cut stream raised openai.APIConnectionError with the pinned SDK in the qualification,
        # but the iteration ending is NOT evidence of completion: a provider can also end a stream
        # cleanly without a terminal event. Only the provider's terminal event is.
        for event in stream:
            if event.type == "response.output_text.delta":
                text += event.delta
            terminal = TERMINALS.get(event.type, terminal)
    if terminal is None:
        raise RuntimeError("the stream ended without a terminal event: truncated, treat the output as partial")
    if terminal != "completed":
        raise RuntimeError(f"the provider declared the response {terminal}: treat the output as unusable")
    print("stream:", text)


try:
    main()
except openai.APIStatusError as err:
    print(f"request failed: status {err.status_code}, code {err.code}", file=sys.stderr)
    raise SystemExit(1)
except Exception as err:  # noqa: BLE001 - report the class only, never request or response text
    print(f"request failed: {type(err).__name__}", file=sys.stderr)
    raise SystemExit(1)
