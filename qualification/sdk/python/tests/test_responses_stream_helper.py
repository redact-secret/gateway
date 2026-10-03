"""The SDK's `responses.stream()` helper (accumulating stream with a final response) against the
Responses relay (#88). Terminal events come from the provider, never from the gateway: a normal
completion yields the final response; a provider-declared failed/incomplete outcome is content; a
clean end without a terminal event and an abrupt cut never yield a completed response. What the
helper RAISES is recorded as evidence. Measured for this pin: get_final_response() accepts only
`response.completed`, so a provider-declared failed/incomplete outcome is visible as the last event
of the iteration and then raises RuntimeError from the SDK (the gateway relayed it unchanged).
"""

import json
import sys
import unittest
from typing import Any

from qual_support import GATEWAYS, STREAM_TEXT, client, provider, secret, write_evidence

OBSERVATIONS: list[dict[str, Any]] = []


def run(model: str, gateway: str = "standard") -> dict[str, Any]:
    out: dict[str, Any] = {"types": [], "final": None, "raised": None}
    try:
        with client(GATEWAYS[gateway]).responses.stream(model=model, input=f"helper {secret(3)}", store=False) as helper:
            for event in helper:
                out["types"].append(event.type)
            out["final"] = helper.get_final_response()
    except Exception as err:  # noqa: BLE001 - observation
        out["raised"] = type(err).__name__
    return out


def tearDownModule() -> None:
    write_evidence("responses-stream-helper-python.json", {"sdk": "openai (PyPI) 3.24.0", "python": sys.version.split()[0], "observations": OBSERVATIONS})


class StreamHelper(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_completed_stream_final_response_and_sanitized_request_sent_once(self) -> None:
        r = run("qual-resp-sse-ok")
        self.assertIsNone(r["raised"])
        self.assertEqual(r["final"].status, "completed")
        self.assertEqual(r["final"].output_text, STREAM_TEXT)
        self.assertEqual((r["types"][0], r["types"][-1]), ("response.created", "response.completed"))
        seen = provider.calls()
        self.assertEqual((len(seen["calls"]), seen["connections"]), (1, 1))
        body = json.loads(seen["calls"][0]["body"])
        self.assertEqual((body["stream"], body["store"], body["input"]), (True, False, "helper <SECRET_1>"))
        OBSERVATIONS.append({"case": "completed", "raised": r["raised"], "final_status": r["final"].status})

    def test_provider_declared_failed_and_incomplete_are_relayed_as_the_last_event(self) -> None:
        for model, status in [("qual-resp-sse-failed", "failed"), ("qual-resp-sse-incomplete", "incomplete")]:
            with self.subTest(model=model):
                provider.reset()
                r = run(model)
                self.assertEqual(r["types"][-1], f"response.{status}")
                self.assertNotIn("response.completed", r["types"])
                self.assertEqual(r["raised"], "RuntimeError", "this SDK pin raises when no response.completed arrived")
                OBSERVATIONS.append({"case": model, "raised": r["raised"], "final_status": r["final"].status if r["final"] else None})

    def test_no_terminal_event_never_yields_a_completed_final_response(self) -> None:
        for model, gateway in [("qual-resp-sse-clean-no-terminal", "standard"), ("qual-resp-sse-truncated", "standard"), ("qual-resp-sse-hang", "tight")]:
            with self.subTest(model=model):
                provider.reset()
                r = run(model, gateway)
                self.assertNotIn("response.completed", r["types"])
                self.assertNotEqual(getattr(r["final"], "status", None), "completed")
                self.assertEqual(len(provider.calls()["calls"]), 1, "no retry or resume")
                OBSERVATIONS.append({"case": model, "raised": r["raised"], "final_status": getattr(r["final"], "status", None), "events": len(r["types"])})


if __name__ == "__main__":
    unittest.main()
