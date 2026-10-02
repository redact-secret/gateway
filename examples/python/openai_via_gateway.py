"""DRAFT: the proxy endpoint is not available until the Alpha 1 MVP (#8).

Do not run this expecting success. Today the gateway skeleton answers
POST /v1/chat/completions with 404 `unsupported_input` and forwards nothing.

Shows the intended shape only: an existing OpenAI SDK pointed at the gateway base URL.
The key below is an obviously fake placeholder. Never put a real key in this file.

Intended setup (not tested here): pip install "openai==<pinned version>" ; python this-file
"""

from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8787/v1",  # the gateway, not the provider
    api_key="SYNTHETIC-PLACEHOLDER-NOT-A-REAL-KEY",
)

try:
    reply = client.chat.completions.create(
        model="placeholder-model",
        messages=[{"role": "user", "content": "hello"}],
    )
    print(reply.choices[0].message.content)
except Exception as err:  # Expected until #8: a 404 from the skeleton.
    print(f"draft example failed (expected until the Alpha 1 MVP): {type(err).__name__}")
    raise SystemExit(1)
