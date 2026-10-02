"""SSE streaming through the pinned Python SDK: fragmented, multibyte, multi-event, slow, gated
(incremental), interrupted, and provider-error streams."""

import json
import time
import unittest
from typing import Any

import openai
from qual_support import GATEWAYS, STREAM_TEXT, client, prompt_marker, provider, secret, write_evidence

MSG = [{"role": "user", "content": "stream please"}]
OBSERVATIONS: list[dict[str, Any]] = []


def drain(model: str, gateway: str = "standard") -> dict[str, Any]:
    text, events, finish, raised = "", 0, None, None
    try:
        stream = client(GATEWAYS[gateway]).chat.completions.create(model=model, messages=MSG, stream=True)
        with stream:
            for chunk in stream:
                events += 1
                if chunk.choices:
                    text += chunk.choices[0].delta.content or ""
                    finish = chunk.choices[0].finish_reason or finish
    except Exception as err:  # noqa: BLE001 - observation, classified below
        raised = type(err).__name__
    return {"text": text, "events": events, "finish": finish, "raised": raised}


def tearDownModule() -> None:
    import sys

    write_evidence(
        "stream-truncation-observations-python.json",
        {"sdk": "openai (PyPI) 3.24.0", "python": sys.version.split()[0], "observations": OBSERVATIONS},
    )


class Streaming(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_events_arrive_intact_and_in_order_in_every_framing(self) -> None:
        for model in ("qual-sse-ok", "qual-sse-multi", "qual-sse-fragmented"):
            with self.subTest(model):
                provider.reset()
                r = drain(model)
                self.assertIsNone(r["raised"])
                self.assertEqual(r["text"], STREAM_TEXT, "multibyte text reassembled exactly")
                self.assertEqual(r["events"], 8, "role event, six text events, final event")
                self.assertEqual(r["finish"], "stop")
                calls = provider.calls()["calls"]
                self.assertEqual(len(calls), 1)
                self.assertIs(json.loads(calls[0]["body"])["stream"], True)

    def test_streamed_request_is_sanitized_like_any_other(self) -> None:
        marker = prompt_marker()
        stream = client(GATEWAYS["standard"]).chat.completions.create(
            model="qual-sse-ok",
            stream=True,
            stream_options={"include_usage": True},
            messages=[{"role": "user", "content": f"{marker} token {secret(41)}"}],
        )
        with stream:
            for _ in stream:
                pass
        body = provider.calls()["calls"][0]["body"]
        self.assertNotIn(secret(41), body)
        self.assertNotIn("ghp_", body)
        self.assertIn("<SECRET_1>", body)
        self.assertIn(marker, body)
        self.assertEqual(json.loads(body)["stream_options"], {"include_usage": True})

    def test_relays_incrementally_while_the_provider_holds_the_stream(self) -> None:
        stream = client(GATEWAYS["standard"]).chat.completions.create(model="qual-sse-gated", messages=MSG, stream=True)
        with stream:
            iterator = iter(stream)
            next(iterator)  # an early event reaches the SDK ...
            self.assertTrue(provider.await_event(1, "gated"), "provider is holding the rest of the stream")
            held = provider.calls()["calls"][0]
            self.assertNotIn("finished", held["events"], "... while the provider has not finished: no buffering")
            provider.release(1)
            text, seen = "", 1
            for chunk in iterator:
                seen += 1
                if chunk.choices:
                    text += chunk.choices[0].delta.content or ""
            self.assertEqual(seen, 8)
            self.assertEqual(text, STREAM_TEXT)
        self.assertTrue(provider.await_event(1, "finished"))

    def test_slow_provider_stream_is_spread_over_time(self) -> None:
        stream = client(GATEWAYS["standard"]).chat.completions.create(model="qual-sse-slow", messages=MSG, stream=True)
        arrivals = []
        with stream:
            for _ in stream:
                arrivals.append(time.monotonic())
        self.assertEqual(len(arrivals), 8)
        self.assertGreaterEqual(arrivals[-1] - arrivals[0], 0.1, "events are paced 25 ms apart; the relay must not batch them")

    def test_interrupted_provider_stream_is_an_sdk_error_without_fabricated_completion(self) -> None:
        r = drain("qual-sse-interrupted")
        OBSERVATIONS.append({"case": "provider cut the stream after two events", **r})
        self.assertIsNotNone(r["raised"], "the truncated stream must surface as an error, not as a normal end")
        self.assertIsNone(r["finish"], "no completion was fabricated")
        self.assertLessEqual(r["events"], 2)

    def test_stalled_provider_stream_is_cut_by_the_gateway_idle_deadline(self) -> None:
        r = drain("qual-sse-hang", "tight")
        OBSERVATIONS.append({"case": "gateway idle deadline cut the stream", **r})
        self.assertIsNotNone(r["raised"])
        self.assertIsNone(r["finish"])
        self.assertTrue(provider.await_event(1, "closed"), "gateway closed the provider connection")

    def test_provider_error_before_the_stream_starts_is_a_normal_api_error(self) -> None:
        with self.assertRaises(openai.RateLimitError) as caught:
            client(GATEWAYS["standard"]).chat.completions.create(model="qual-sse-err-429", messages=MSG, stream=True)
        self.assertEqual(caught.exception.status_code, 429)


if __name__ == "__main__":
    unittest.main()
