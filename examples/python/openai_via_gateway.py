"""An existing OpenAI SDK pointed at the local RedactSecret Gateway (Alpha 1).

What this does: one ordinary Chat Completions call and one streamed call, both sent to the
gateway's base URL instead of api.openai.com. The gateway inspects the prompt text with the
RedactSecret core, forwards only the sanitized request to OpenAI, and relays the answer
unchanged. Responses are NOT redacted.

You supply a real provider key at runtime in OPENAI_API_KEY. It is sent to the gateway as the
usual Bearer token and forwarded only to the provider. Never put a key in this file, in a config
file, or in version control.

    cd examples/python
    python3 -m venv .venv
    .venv/bin/pip install --require-hashes --no-deps -r requirements.txt
    export OPENAI_API_KEY=...            # your own key, not stored anywhere by the gateway
    .venv/bin/python openai_via_gateway.py
    .venv/bin/python openai_via_gateway.py --demo-redaction   # also sends a synthetic token

Environment: GATEWAY_BASE_URL (default http://127.0.0.1:8787/v1), EXAMPLE_MODEL (default
gpt-4o-mini; use a chat model your account can call).

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

# max_retries=0 so a failed request is never silently sent to the provider a second time. The SDK
# default is 2 retries for 408/409/429/5xx and connection errors, and a retry of a request that
# already reached the provider can duplicate (and bill) provider-side work. Raise it only if you
# accept that. See docs/contracts/errors-and-telemetry.md ("SDK retry guidance").
client = OpenAI(base_url=base_url, api_key=api_key, max_retries=0)

# Synthetic and revoked-looking. It is not a credential; the gateway replaces it with a
# placeholder before anything leaves your machine.
SYNTHETIC_TOKEN = "ghp_SYNTHETICREVOKED00000000000000000001"

prompt = (
    f"My (synthetic) token is {SYNTHETIC_TOKEN}. Reply with one short greeting."
    if demo_redaction
    else "Reply with one short greeting."
)


def main() -> None:
    reply = client.chat.completions.create(model=model, messages=[{"role": "user", "content": prompt}])
    print("completion:", reply.choices[0].message.content)

    text, finish_reason = "", None
    with client.chat.completions.create(
        model=model, messages=[{"role": "user", "content": prompt}], stream=True
    ) as stream:
        # A broken stream raises openai.APIConnectionError here: the gateway ends it abruptly and
        # never invents a completion. Checking finish_reason is still good practice.
        for chunk in stream:
            if chunk.choices:
                text += chunk.choices[0].delta.content or ""
                finish_reason = chunk.choices[0].finish_reason or finish_reason
    if finish_reason is None:
        raise RuntimeError("the stream ended without a completion event: treat the output as truncated")
    print("stream:", text)


try:
    main()
except openai.APIStatusError as err:
    # The gateway's own errors carry a fixed safe code (for example unsupported_input).
    print(f"request failed: status {err.status_code}, code {err.code}", file=sys.stderr)
    raise SystemExit(1)
except Exception as err:  # noqa: BLE001 - report the class only, never request or response text
    print(f"request failed: {type(err).__name__}", file=sys.stderr)
    raise SystemExit(1)
