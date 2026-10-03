"""Both endpoints under concurrent mixed load with request-local state and independent provider
credentials (#88), through the pinned Python SDK.

28 requests at once per wave against ONE gateway instance, alternating POST /v1/chat/completions
and POST /v1/responses, mixing redacted, blocked, warn-rejected, label-rejected, provider-declared
incomplete and streamed outcomes. Every request carries a unique marker and its OWN provider key.
The oracle is the fake provider's record: each forwarded request must appear exactly once, on its
own endpoint path and connection, with its own key's Authorization hash, its own secrets numbered
from <SECRET_1> and nothing of any neighbour; every rejected request must be absent from the
record. Status 200 alone is not an oracle.
"""

import hashlib
import json
import re
import sys
import unittest
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Any

import openai
from qual_support import GATEWAYS, STREAM_TEXT, SYN, leaks, provider, secret, write_evidence

PEM = "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----"
WARN = "password=hunter2xyz"
FORWARDING = {"chat-redact", "chat-stream", "resp-redact", "resp-tool-history", "resp-stream", "resp-stream-incomplete", "resp-incomplete-json"}
PATTERN = [
    "resp-redact", "chat-redact", "resp-block", "resp-tool-history", "chat-block", "resp-stream", "resp-label",
    "chat-stream", "resp-warn", "resp-incomplete-json", "chat-warn", "resp-redact", "resp-store-missing", "chat-redact",
    "resp-stream-incomplete", "resp-block", "chat-redact", "resp-tool-history", "resp-label", "chat-stream", "resp-warn",
    "resp-redact", "chat-block", "resp-incomplete-json", "resp-stream", "chat-warn", "resp-store-missing", "chat-redact",
]
ROWS: list[dict[str, Any]] = []
COUNTER = [1000]


@dataclass
class Job:
    i: int
    kind: str
    marker: str
    key: str
    secrets: list[int]

    @property
    def forwards(self) -> bool:
        return self.kind in FORWARDING

    @property
    def path(self) -> str:
        return "/v1/chat/completions" if self.kind.startswith("chat") else "/v1/responses"

    @property
    def text(self) -> str:
        return f"{self.marker} " + " ".join(f"tok {secret(s)}" for s in self.secrets)


def make_jobs() -> list[Job]:
    jobs = []
    for i, kind in enumerate(PATTERN):
        n = 1 + (i % 3)
        ids = list(range(COUNTER[0], COUNTER[0] + n))
        COUNTER[0] += n
        jobs.append(Job(i, kind, f"{SYN['prompt_marker_prefix']}{uuid.uuid4()}", f"{SYN['api_key']}-{i}", ids))
    return jobs


def sha(key: str) -> str:
    return hashlib.sha256(f"Bearer {key}".encode()).hexdigest()


def send(j: Job) -> tuple[int, bool]:
    # Bounded admission may answer 503 overload to a burst; such a request never reached the provider
    # and the SDK retries it after the relayed wait, so the delivery count stays exact.
    c = openai.OpenAI(base_url=f"{GATEWAYS['concurrent']}/v1", api_key=j.key, max_retries=4)
    try:
        k = j.kind
        if k == "chat-redact":
            return 200, c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": j.text}]).choices[0].message.content == STREAM_TEXT
        if k == "chat-stream":
            finish = None
            with c.chat.completions.create(model="qual-sse-ok", messages=[{"role": "user", "content": j.text}], stream=True) as s:
                for chunk in s:
                    if chunk.choices:
                        finish = chunk.choices[0].finish_reason or finish
            return 200, finish == "stop"
        if k == "chat-block":
            c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": f"{j.marker} {PEM}"}])
        if k == "chat-warn":
            c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": f"{j.marker} {WARN}"}])
        if k == "resp-redact":
            return 200, c.responses.create(model="qual-resp-json-ok", input=j.text, instructions=f"i {j.marker}", store=False).output_text == STREAM_TEXT
        if k == "resp-tool-history":
            history = [
                {"role": "user", "content": j.text},
                {"type": "function_call", "call_id": f"call_{j.i}", "name": "lookup", "arguments": json.dumps({"q": j.marker})},
                {"type": "function_call_output", "call_id": f"call_{j.i}", "output": "result"},
            ]
            return 200, c.responses.create(model="qual-resp-json-ok", input=history, store=False).output_text == STREAM_TEXT
        if k in ("resp-stream", "resp-stream-incomplete"):
            want = "response.completed" if k == "resp-stream" else "response.incomplete"
            model = "qual-resp-sse-ok" if k == "resp-stream" else "qual-resp-sse-incomplete"
            seen = []
            with c.responses.create(model=model, input=j.text, store=False, stream=True) as s:
                seen = [e.type for e in s]
            return 200, seen[-1] == want
        if k == "resp-incomplete-json":
            return 200, c.responses.create(model="qual-resp-json-incomplete", input=j.text, store=False).status == "incomplete"
        if k == "resp-block":
            c.responses.create(model="qual-resp-json-ok", input=f"{j.marker} {PEM}", store=False)
        if k == "resp-warn":
            c.responses.create(model="qual-resp-json-ok", input=f"{j.marker} {WARN}", store=False)
        if k == "resp-label":
            c.responses.create(model="qual-resp-json-ok", input=j.marker, store=False, tools=[{"type": "function", "name": secret(j.secrets[0]), "parameters": None, "strict": None}])
        if k == "resp-store-missing":
            c.responses.create(model="qual-resp-json-ok", input=j.marker)
        return 200, False
    except openai.APIStatusError as err:
        assert leaks(json.dumps({"m": err.message, "b": err.body}, default=str)) == [], f"{j.kind}: error text leaks"
        return err.status_code, False


class Mixed(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def run_wave(self, wave: int) -> None:
        jobs = make_jobs()
        with ThreadPoolExecutor(max_workers=len(jobs)) as pool:
            results = list(pool.map(send, jobs))
        seen = provider.calls()
        calls = seen["calls"]
        forwarded = [j for j in jobs if j.forwards]
        self.assertEqual(len(calls), len(forwarded), "exactly one delivery per forwarded request, none for rejected ones")
        self.assertEqual(seen["connections"], len(forwarded), "one upstream connection per delivery")
        for j, (status, ok) in zip(jobs, results):
            mine = [c for c in calls if j.marker in c["body"]]
            if not j.forwards:
                self.assertEqual(mine, [], f"{j.kind} #{j.i} reached the provider")
                self.assertEqual(status, 422, f"{j.kind} #{j.i}: status {status}")
                continue
            self.assertTrue(ok, f"{j.kind} #{j.i}: the provider's outcome was relayed")
            self.assertEqual(len(mine), 1, f"{j.kind} #{j.i}: delivered exactly once")
            call = mine[0]
            self.assertEqual(call["path"], j.path, f"{j.kind} #{j.i}: own endpoint")
            self.assertEqual(call["authorization_sha256"], sha(j.key), f"{j.kind} #{j.i}: own provider credential")
            self.assertNotIn("ghp_SYNTH", call["body"], f"{j.kind} #{j.i}: a secret reached the provider")
            numbers = sorted({int(m) for m in re.findall(r"<SECRET_(\d+)>", call["body"])})
            self.assertEqual(numbers, list(range(1, len(j.secrets) + 1)), f"{j.kind} #{j.i}: numbering starts at 1 for this request alone")
            for other in jobs:
                if other is not j:
                    self.assertNotIn(other.marker, call["body"], f"{j.kind} #{j.i} carries marker of #{other.i}")
        self.assertEqual(len({c["authorization_sha256"] for c in calls}), len(calls))
        self.assertTrue(all(not c["local_token_seen"] for c in calls))
        ROWS.append(
            {
                "wave": wave,
                "requests": len(jobs),
                "forwarded": len(forwarded),
                "rejected": len(jobs) - len(forwarded),
                "chat_deliveries": sum(1 for c in calls if c["path"] == "/v1/chat/completions"),
                "responses_deliveries": sum(1 for c in calls if c["path"] == "/v1/responses"),
                "connections": seen["connections"],
            }
        )

    def test_wave_1(self) -> None:
        self.run_wave(1)

    def test_wave_2(self) -> None:
        self.run_wave(2)

    def test_after_the_load_the_gateway_still_serves_both_endpoints(self) -> None:
        c = openai.OpenAI(base_url=f"{GATEWAYS['concurrent']}/v1", api_key=SYN["api_key"], max_retries=0)
        self.assertEqual(c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": "after"}]).choices[0].message.content, STREAM_TEXT)
        self.assertEqual(c.responses.create(model="qual-resp-json-ok", input="after", store=False).output_text, STREAM_TEXT)

    @classmethod
    def tearDownClass(cls) -> None:
        write_evidence("responses-mixed-python.json", {"sdk": f"openai (PyPI) {openai.__version__}", "python": sys.version.split()[0], "waves": ROWS})


if __name__ == "__main__":
    unittest.main()
