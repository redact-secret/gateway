"""Client abort cancels the upstream exchange, and repeated cancellation leaks no capacity.

The standard gateway has 8 upstream and 8 stream permits; 12 aborted exchanges followed by a
normal request prove every permit came back (a leak would exhaust capacity first).
"""

import unittest

import openai
from qual_support import GATEWAYS, STREAM_TEXT, client, provider

MSG = [{"role": "user", "content": "cancel me"}]


def abort_stream(seq: int) -> None:
    stream = client(GATEWAYS["standard"]).chat.completions.create(model="qual-sse-hang", messages=MSG, stream=True)
    iterator = iter(stream)
    next(iterator)
    stream.close()  # client disconnects mid-stream


class Cancellation(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_client_timeout_before_the_provider_answers_closes_the_upstream_connection(self) -> None:
        with self.assertRaises(openai.APITimeoutError):
            client(GATEWAYS["standard"], timeout=0.3).chat.completions.create(model="qual-hang", messages=MSG)
        self.assertTrue(provider.await_event(1, "received"))
        self.assertTrue(provider.await_event(1, "closed"), "the gateway cancelled the upstream exchange")

    def test_closing_an_sse_stream_mid_flight_closes_the_upstream_stream(self) -> None:
        abort_stream(1)
        self.assertTrue(provider.await_event(1, "closed"), "provider connection closed after the client went away")

    def test_repeated_cancellation_returns_every_permit(self) -> None:
        for i in range(1, 13):
            with self.assertRaises(openai.APITimeoutError):
                client(GATEWAYS["standard"], timeout=0.3).chat.completions.create(model="qual-hang", messages=MSG)
            self.assertTrue(provider.await_event(i, "closed"), f"JSON cancel {i} reached the provider as a close")
        base = len(provider.calls()["calls"])
        for i in range(1, 13):
            abort_stream(i)
            self.assertTrue(provider.await_event(base + i, "closed"), f"SSE cancel {i} reached the provider as a close")
        ok = client(GATEWAYS["standard"]).chat.completions.create(model="qual-json-ok", messages=MSG)
        self.assertEqual(ok.choices[0].message.content, STREAM_TEXT, "capacity fully returned")
        events = 0
        with client(GATEWAYS["standard"]).chat.completions.create(model="qual-sse-ok", messages=MSG, stream=True) as s:
            for _ in s:
                events += 1
        self.assertEqual(events, 8)


if __name__ == "__main__":
    unittest.main()
