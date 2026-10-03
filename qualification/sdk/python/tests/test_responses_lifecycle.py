"""Responses (`POST /v1/responses`) JSON and SSE relay lifecycle through the pinned Python SDK (#87).

The terminal signal of a Responses stream is the provider's own event: `response.completed`,
`response.failed` or `response.incomplete`. It is not Chat's `finish_reason` and not `[DONE]`.
Three different things can end an iteration and a caller has to tell them apart: a terminal event
the provider sent (completed, or a provider-declared failed/incomplete), a clean transport end with
no terminal event, and an abrupt transport cut. The gateway adds no event, status or retry in any
case. Whether the SDK raises on a cut is recorded as evidence; the asserted invariant is that no
terminal event is ever fabricated.
"""

import json
import sys
import unittest
from typing import Any

import openai
from qual_support import GATEWAYS, STREAM_TEXT, client, prompt_marker, provider, secret, write_evidence

TERMINALS = {"response.completed": "completed", "response.failed": "failed", "response.incomplete": "incomplete"}
OBSERVATIONS: list[dict[str, Any]] = []


def collect(model: str, gateway: str = "standard") -> dict[str, Any]:
    out: dict[str, Any] = {"terminal": None, "types": [], "text": "", "args": "", "raised": None}
    try:
        stream = client(GATEWAYS[gateway]).responses.create(model=model, input="stream please", store=False, stream=True)
        with stream:
            for event in stream:
                out["types"].append(event.type)
                if event.type == "response.output_text.delta":
                    out["text"] += event.delta
                if event.type == "response.function_call_arguments.delta":
                    out["args"] += event.delta
                out["terminal"] = TERMINALS.get(event.type, out["terminal"])
    except Exception as err:  # noqa: BLE001 - observation, recorded below
        out["raised"] = type(err).__name__
    return out


def tearDownModule() -> None:
    write_evidence(
        "responses-lifecycle-observations-python.json",
        {"sdk": "openai (PyPI) 3.24.0", "python": sys.version.split()[0], "observations": OBSERVATIONS},
    )


class ResponsesLifecycle(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_completed_stream_is_intact_sanitized_and_sent_once_to_the_responses_path(self) -> None:
        marker = prompt_marker()
        r = {"types": [], "text": "", "terminal": None}
        stream = client(GATEWAYS["standard"]).responses.create(
            model="qual-resp-sse-ok", input=f"{marker} token {secret(87)}", store=False, stream=True
        )
        with stream:
            for event in stream:
                r["types"].append(event.type)
                if event.type == "response.output_text.delta":
                    r["text"] += event.delta
                r["terminal"] = TERMINALS.get(event.type, r["terminal"])
        self.assertEqual(r["text"], STREAM_TEXT)
        self.assertEqual(r["terminal"], "completed")
        self.assertEqual(r["types"][0], "response.created")
        self.assertEqual(r["types"][-1], "response.completed")
        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["path"], "/v1/responses")
        body = calls[0]["body"]
        self.assertNotIn(secret(87), body)
        self.assertNotIn("ghp_", body)
        self.assertIn("<SECRET_1>", body)
        self.assertIn(marker, body)
        self.assertIs(json.loads(body)["stream"], True)
        self.assertIs(json.loads(body)["store"], False)

    def test_function_call_arguments_arrive_in_order_and_the_stream_completes(self) -> None:
        r = collect("qual-resp-sse-tool")
        self.assertIsNone(r["raised"])
        self.assertEqual(json.loads(r["args"]), {"city": "서울"})
        self.assertEqual(r["terminal"], "completed")

    def test_provider_declared_failed_and_incomplete_are_the_providers_own_terminal_event(self) -> None:
        for model, expected in (("qual-resp-sse-failed", "failed"), ("qual-resp-sse-incomplete", "incomplete")):
            with self.subTest(model):
                provider.reset()
                r = collect(model)
                self.assertEqual(r["terminal"], expected)
                self.assertEqual(r["types"][-1], f"response.{expected}")
                self.assertNotIn("response.completed", r["types"], "no completion was fabricated")
                self.assertIsNone(r["raised"], "a provider-declared outcome is content; the transport ended normally")
                self.assertEqual(len(provider.calls()["calls"]), 1, "the gateway never retries")
                OBSERVATIONS.append({"case": model, "terminal": r["terminal"], "sdk_raised": r["raised"]})

    def test_no_terminal_event_is_ever_fabricated(self) -> None:
        cases = (
            ("qual-resp-sse-clean-no-terminal", "standard", "provider ended the stream cleanly without a terminal event"),
            ("qual-resp-sse-truncated", "standard", "provider dropped the connection mid-stream"),
            ("qual-resp-sse-hang", "tight", "provider went silent; the gateway's idle deadline cut the stream"),
        )
        for model, gateway, why in cases:
            with self.subTest(model):
                provider.reset()
                r = collect(model, gateway)
                self.assertIsNone(r["terminal"], "the response is NOT complete")
                for forbidden in ("response.completed", "response.failed", "error"):
                    self.assertNotIn(forbidden, r["types"])
                self.assertLessEqual(len(r["types"]), 2, "only the events the provider really sent arrived")
                self.assertEqual(len(provider.calls()["calls"]), 1, "the gateway never retries or resumes")
                OBSERVATIONS.append(
                    {"case": model, "why": why, "terminal": r["terminal"], "events": len(r["types"]), "sdk_raised": r["raised"]}
                )

    def test_client_abort_after_the_headers_closes_the_provider_exchange(self) -> None:
        stream = client(GATEWAYS["standard"]).responses.create(
            model="qual-resp-sse-hang", input="abort me", store=False, stream=True
        )
        iterator = iter(stream)
        next(iterator)
        stream.close()
        self.assertTrue(provider.await_event(1, "closed"), "the gateway dropped the provider connection")
        self.assertEqual(len(provider.calls()["calls"]), 1)

    def test_ordinary_json_completed_failed_and_incomplete_are_returned_unchanged(self) -> None:
        c = client(GATEWAYS["standard"]).responses
        ok = c.create(model="qual-resp-json-ok", input="hi", store=False)
        self.assertEqual(ok.status, "completed")
        self.assertEqual(ok.output_text, STREAM_TEXT)
        failed = c.create(model="qual-resp-json-failed", input="hi", store=False)
        self.assertEqual(failed.status, "failed")
        self.assertEqual(failed.error.code, "server_error")
        incomplete = c.create(model="qual-resp-json-incomplete", input="hi", store=False)
        self.assertEqual(incomplete.status, "incomplete")
        self.assertEqual(incomplete.incomplete_details.reason, "max_output_tokens")
        self.assertEqual(len(provider.calls()["calls"]), 3)

    def test_provider_error_status_is_a_normal_sdk_error(self) -> None:
        with self.assertRaises(openai.RateLimitError) as caught:
            client(GATEWAYS["standard"]).responses.create(model="qual-resp-err-429", input="hi", store=False)
        self.assertEqual(caught.exception.status_code, 429)
        self.assertEqual(len(provider.calls()["calls"]), 1)

    def test_rejected_request_never_reaches_the_provider(self) -> None:
        with self.assertRaises(openai.APIStatusError) as caught:
            client(GATEWAYS["standard"]).responses.create(model="qual-resp-json-ok", input="hi")
        self.assertTrue(400 <= caught.exception.status_code < 500)
        seen = provider.calls()
        self.assertEqual(seen["connections"], 0)
        self.assertEqual(len(seen["calls"]), 0)


if __name__ == "__main__":
    unittest.main()
