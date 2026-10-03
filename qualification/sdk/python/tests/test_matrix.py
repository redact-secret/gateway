"""Alpha 2 transport qualification matrix (#60), Python SDK. Mirrors qualification/sdk/node/test/matrix.test.ts.

Every scenario runs twice: with the SDK's default retries (max_retries = 2, what an unconfigured app
gets) and with retries explicitly disabled (max_retries=0). For each run the matrix records the HTTP
attempts the SDK made to the gateway, the requests and request bodies that reached the (fake)
provider, whether those bodies were identical (a client retry is a NEW request: identical bodies are
duplicate provider-side work, not a gateway replay, because the gateway never retries), and for
streams how many events arrived, whether `finish_reason` was seen, and whether the SDK raised.
The completion verdict a caller can rely on is `finish_reason` seen, never "the iteration ended".
"""

import sys
import unittest
from typing import Any

import openai
from qual_support import GATEWAYS, SYN, closed_port, counting_http_client, provider, write_evidence

RUNS: list[dict[str, Any]] = []
DEFAULT = "default(2)"


def run(scenario: str, base: str, model: str, stream: bool, max_retries: Any, content: str = "matrix observation") -> dict[str, Any]:
    provider.reset()
    http, attempts = counting_http_client()
    kwargs: dict[str, Any] = {}
    if max_retries == 0:
        kwargs["max_retries"] = 0
    c = openai.OpenAI(base_url=f"{base}/v1", api_key=SYN["api_key"], http_client=http, **kwargs)
    messages = [{"role": "user", "content": content}]
    outcome = "ok"
    stream_info = None
    try:
        if stream:
            events, finish, raised = 0, False, False
            result = c.chat.completions.create(model=model, messages=messages, stream=True)
            try:
                with result:
                    for chunk in result:
                        events += 1
                        if chunk.choices and chunk.choices[0].finish_reason:
                            finish = True
            except Exception as err:  # noqa: BLE001 - observation
                raised = True
                outcome = f"stream error {type(err).__name__}"
            stream_info = {
                "events": events,
                "finish_reason_seen": finish,
                "sdk_raised": raised,
                "verdict": "complete" if finish else "incomplete",
            }
        else:
            c.chat.completions.create(model=model, messages=messages)
    except openai.APIStatusError as err:
        outcome = f"status {err.status_code}"
    except Exception as err:  # noqa: BLE001 - observation
        outcome = f"error {type(err).__name__}"
    seen = provider.calls()["calls"]
    bodies = [call["body"] for call in seen if call["body_bytes"] > 0]
    record = {
        "scenario": scenario,
        "sdk_max_retries": 0 if max_retries == 0 else DEFAULT,
        "sdk_attempts": len(attempts),
        "sdk_outcome": outcome,
        "provider_requests": len(seen),
        "provider_bodies_received": len(bodies),
        "provider_bodies_identical": bool(bodies) and all(b == bodies[0] for b in bodies),
        "provider_body_bytes_received": [call["body_bytes"] for call in seen],
        "stream": stream_info,
    }
    RUNS.append(record)
    return record


def tearDownModule() -> None:
    write_evidence(
        "matrix-observations-python.json",
        {
            "sdk": "openai (PyPI) 3.24.0",
            "python": sys.version.split()[0],
            "note": "provider_requests counts what reached the fake provider; a client retry is a new request, so equal counts with identical bodies are duplicate provider-side work. The gateway never retries.",
            "runs": RUNS,
        },
    )


DUPLICATING = [
    ("provider closes before any response byte", "qual-close-before-headers"),
    ("provider sends a partial response head, then closes", "qual-partial-head"),
    ("provider answers with a non-HTTP status line", "qual-bad-status-line"),
    ("provider sends two different Content-Length values", "qual-dup-content-length"),
    ("provider applies Content-Encoding: gzip", "qual-gzip"),
    ("provider truncates the JSON body", "qual-json-truncated"),
    ("provider response over the body bound", "qual-json-oversize"),
    ("provider stops reading the request body (partial upstream send)", "qual-partial-read"),
]

# (label, model, gateway, finish_reason expected, SDK raises expected)
STREAMS = [
    ("complete stream", "qual-sse-ok", "standard", True, False),
    ("finish_reason but no [DONE], clean end", "qual-sse-no-done", "standard", True, False),
    ("clean end after two events: no finish_reason, no [DONE]", "qual-sse-clean-no-finish", "standard", False, False),
    ("provider cut after two events", "qual-sse-interrupted", "standard", False, True),
    ("provider cut after the headers, before any event", "qual-sse-cut-before-first-event", "standard", False, True),
    ("invalid chunk framing after the headers", "qual-sse-bad-chunk", "standard", False, True),
    ("stalled provider stream cut by the gateway idle deadline", "qual-sse-hang", "tight", False, True),
]


class Matrix(unittest.TestCase):
    def test_provider_failures_become_502_and_default_retries_duplicate_provider_work(self) -> None:
        for label, model in DUPLICATING:
            with self.subTest(label):
                content = "partial upstream send " * 9000 if model == "qual-partial-read" else "matrix observation"
                default = run(label, GATEWAYS["standard"], model, False, "default", content)
                none = run(label, GATEWAYS["standard"], model, False, 0, content)
                self.assertEqual((none["sdk_attempts"], none["provider_requests"]), (1, 1))
                self.assertEqual((default["sdk_attempts"], default["provider_requests"]), (3, 3))
                self.assertEqual(default["sdk_outcome"], "status 502")
                self.assertEqual(none["sdk_outcome"], "status 502")
                if model == "qual-partial-read":
                    full = len('{"content":"' + content + '"}')
                    self.assertTrue(all(0 < n < full for n in default["provider_body_bytes_received"]), "provider read only part of the body")
                else:
                    self.assertTrue(default["provider_bodies_identical"], "the same sanitized body, sent three times as three requests")

    def test_provider_never_answers_is_504_and_default_retries_triple_the_work(self) -> None:
        default = run("provider never answers", GATEWAYS["tight"], "qual-hang", False, "default")
        none = run("provider never answers", GATEWAYS["tight"], "qual-hang", False, 0)
        self.assertEqual((default["sdk_attempts"], default["provider_requests"]), (3, 3))
        self.assertEqual((none["sdk_attempts"], none["provider_requests"]), (1, 1))
        self.assertEqual(default["sdk_outcome"], "status 504")

    def test_gateway_overload_is_retried_after_the_relayed_wait_and_never_reaches_the_provider(self) -> None:
        provider.reset()
        holder = openai.OpenAI(base_url=f"{GATEWAYS['overload']}/v1", api_key=SYN["api_key"], max_retries=0)
        stream = holder.chat.completions.create(model="qual-sse-hang", messages=[{"role": "user", "content": "hold"}], stream=True)
        iterator = iter(stream)
        next(iterator)
        none = run("gateway overload", GATEWAYS["overload"], "qual-json-ok", False, 0)
        default = run("gateway overload", GATEWAYS["overload"], "qual-json-ok", False, "default")
        stream.close()
        self.assertEqual((none["sdk_attempts"], none["provider_requests"]), (1, 0))
        self.assertEqual((default["sdk_attempts"], default["provider_requests"]), (3, 0))
        self.assertEqual(default["sdk_outcome"], "status 503")

    def test_connection_refused_is_retried_by_default_and_nothing_reaches_the_provider(self) -> None:
        base = f"http://127.0.0.1:{closed_port()}"
        default = run("gateway connection refused", base, "qual-json-ok", False, "default")
        none = run("gateway connection refused", base, "qual-json-ok", False, 0)
        self.assertEqual((default["sdk_attempts"], default["provider_requests"]), (3, 0))
        self.assertEqual((none["sdk_attempts"], none["provider_requests"]), (1, 0))
        self.assertEqual(default["sdk_outcome"], "error APIConnectionError")

    def test_stream_failing_before_the_headers_is_a_502_and_retried_by_default(self) -> None:
        default = run("stream, provider closes before headers", GATEWAYS["standard"], "qual-close-before-headers", True, "default")
        none = run("stream, provider closes before headers", GATEWAYS["standard"], "qual-close-before-headers", True, 0)
        self.assertEqual((default["sdk_attempts"], default["provider_requests"]), (3, 3))
        self.assertEqual((none["sdk_attempts"], none["provider_requests"]), (1, 1))
        self.assertEqual(default["sdk_outcome"], "status 502")

    def test_streams_after_the_headers_are_never_retried_and_completion_needs_finish_reason(self) -> None:
        for label, model, gateway, finish, raises in STREAMS:
            with self.subTest(label):
                for retries in ("default", 0):
                    r = run(f"stream: {label}", GATEWAYS[gateway], model, True, retries)
                    self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 1), "never retried once the headers are committed")
                    self.assertEqual(r["stream"]["finish_reason_seen"], finish)
                    self.assertEqual(r["stream"]["verdict"], "complete" if finish else "incomplete")
                    self.assertEqual(r["stream"]["sdk_raised"], raises)

    def test_a_clean_end_without_finish_reason_is_not_an_error_to_the_sdk(self) -> None:
        r = run("clean end without finish_reason", GATEWAYS["standard"], "qual-sse-clean-no-finish", True, 0)
        self.assertFalse(r["stream"]["sdk_raised"])
        self.assertFalse(r["stream"]["finish_reason_seen"])
        self.assertLess(r["stream"]["events"], 8)


if __name__ == "__main__":
    unittest.main()
