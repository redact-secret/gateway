"""OBSERVED retry and cancellation behavior of the pinned Python SDK against POST /v1/responses (#88).

Retries: the SDK default (max_retries 2) and the disabled setting (0) are both measured. Every row
counts the HTTP attempts the SDK made to the gateway (an httpx request hook) and the requests and
connections that reached the fake provider. The gateway never retries on its own, so any extra
provider delivery is an SDK retry. Cancellation: a client timeout or close ends the provider
exchange, returns every permit, and cannot retract bytes already delivered.
"""

import json
import sys
import unittest
from typing import Any

import openai
from qual_support import (
    DIRECT_PROVIDER,
    GATEWAYS,
    STREAM_TEXT,
    SYN,
    closed_port,
    client,
    counting_http_client,
    provider,
    secret,
    write_evidence,
)

ROWS: list[dict[str, Any]] = []
BASE: dict[str, Any] = {"input": "retry observation", "store": False}


def observe(label: str, base: str, max_retries: int | None = None, **params: Any) -> dict[str, Any]:
    provider.reset()
    http, attempts = counting_http_client()
    kwargs: dict[str, Any] = {} if max_retries is None else {"max_retries": max_retries}
    c = openai.OpenAI(base_url=f"{base}/v1", api_key=SYN["api_key"], http_client=http, **kwargs)
    outcome = "ok"
    try:
        result = c.responses.create(**params)
        if params.get("stream") is True:
            with result:
                for _ in result:
                    pass
    except openai.APIStatusError as err:
        outcome = f"status {err.status_code}"
    except Exception as err:  # noqa: BLE001 - observation
        outcome = f"error {type(err).__name__}"
    seen = provider.calls()
    row = {
        "case": label,
        "max_retries": "default (2)" if max_retries is None else str(max_retries),
        "sdk_outcome": outcome,
        "sdk_attempts": len(attempts),
        "provider_requests": len(seen["calls"]),
        "provider_connections": seen["connections"],
    }
    ROWS.append(row)
    return row


def tearDownModule() -> None:
    write_evidence(
        "responses-retries-python.json",
        {
            "sdk": "openai (PyPI) 3.24.0",
            "python": sys.version.split()[0],
            "note": "attempts are HTTP requests the SDK sent to the gateway; provider_requests are those that reached the fake provider",
            "rows": ROWS,
        },
    )


class RetriesDefault(unittest.TestCase):
    def test_provider_statuses(self) -> None:
        table = {400: False, 401: False, 403: False, 404: False, 408: True, 409: True, 422: False, 429: True, 500: True, 502: True, 503: True, 504: True}
        for status, retried in table.items():
            with self.subTest(status=status):
                r = observe(f"provider {status}", GATEWAYS["concurrent"], model=f"qual-resp-err-{status}", **BASE)
                self.assertEqual(r["sdk_attempts"], 3 if retried else 1)
                self.assertEqual(r["provider_requests"], r["sdk_attempts"], "one provider delivery per SDK attempt, none added by the gateway")

    def test_provider_429_with_retry_after_zero_is_retried(self) -> None:
        self.assertEqual(observe("provider 429 + Retry-After 0", GATEWAYS["concurrent"], model="qual-resp-err-429-retry-after", **BASE)["sdk_attempts"], 3)

    def test_provider_retry_hints_are_dropped_by_the_gateway(self) -> None:
        direct = observe("provider 500 + x-should-retry false (direct)", DIRECT_PROVIDER, model="qual-resp-err-500-hint-no-retry", **BASE)
        self.assertEqual(direct["sdk_attempts"], 1)
        via = observe("provider 500 + x-should-retry false (through the gateway)", GATEWAYS["concurrent"], model="qual-resp-err-500-hint-no-retry", **BASE)
        self.assertEqual(via["sdk_attempts"], 3)

    def test_two_429s_then_success_three_identical_sanitized_deliveries(self) -> None:
        provider.reset()
        http, attempts = counting_http_client()
        c = openai.OpenAI(base_url=f"{GATEWAYS['concurrent']}/v1", api_key=SYN["api_key"], http_client=http)
        reply = c.responses.create(model="qual-resp-retry-429-then-ok", input=f"retry {secret(5)}", store=False)
        self.assertEqual(reply.output_text, STREAM_TEXT)
        seen = provider.calls()
        self.assertEqual((len(attempts), len(seen["calls"]), seen["connections"]), (3, 3, 3))
        self.assertEqual(len({x["body"] for x in seen["calls"]}), 1, "every delivery carried the same sanitized body")
        self.assertIn("<SECRET_1>", seen["calls"][0]["body"])
        self.assertNotIn("ghp_", seen["calls"][0]["body"])
        ROWS.append({"case": "provider 429, 429, then 200", "max_retries": "default (2)", "sdk_outcome": "ok", "sdk_attempts": 3, "provider_requests": 3, "provider_connections": 3})

    def test_gateway_generated_errors(self) -> None:
        r = observe("gateway 422 unsupported_input", GATEWAYS["concurrent"], model="qual-resp-json-ok", input="x")
        self.assertEqual((r["sdk_attempts"], r["provider_requests"], r["provider_connections"]), (1, 0, 0))
        r = observe("gateway 400 malformed_input", GATEWAYS["concurrent"], model="qual-resp-json-ok", store=False, input=[{"type": "function_call", "call_id": "c", "name": "n", "arguments": "{"}])
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 0))
        r = observe("gateway 413 limit_exceeded", GATEWAYS["tight"], model="qual-resp-json-ok", store=False, input=f"{secret(51)} {secret(52)} {secret(53)}")
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 0))
        r = observe("gateway 501 not_implemented", GATEWAYS["no_upstream"], model="qual-resp-json-ok", **BASE)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))
        r = observe("gateway 502 upstream_unavailable", GATEWAYS["dead_provider"], model="qual-resp-json-ok", **BASE)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))
        r = observe("gateway 504 upstream_timeout", GATEWAYS["tight"], model="qual-resp-hang", **BASE)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))

    def test_connection_error_to_the_gateway_is_retried(self) -> None:
        r = observe("connection refused (gateway down)", f"http://127.0.0.1:{closed_port()}", model="qual-resp-json-ok", **BASE)
        self.assertEqual(r["sdk_outcome"], "error APIConnectionError")
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))

    def test_streams(self) -> None:
        r = observe("SSE truncated after headers", GATEWAYS["concurrent"], model="qual-resp-sse-truncated", stream=True, **BASE)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 1))
        r = observe("SSE provider 429 before headers", GATEWAYS["concurrent"], model="qual-resp-err-429", stream=True, **BASE)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))


class RetriesDisabled(unittest.TestCase):
    def test_exactly_one_attempt(self) -> None:
        for label, base, model, upstream in [
            ("provider 429", "concurrent", "qual-resp-err-429", 1),
            ("provider 500", "concurrent", "qual-resp-err-500", 1),
            ("provider 429, 429, then 200 (first 429 is final)", "concurrent", "qual-resp-retry-429-then-ok", 1),
            ("gateway 502 upstream_unavailable", "dead_provider", "qual-resp-json-ok", 0),
            ("gateway 504 upstream_timeout", "tight", "qual-resp-hang", 1),
            ("gateway 501 not_implemented", "no_upstream", "qual-resp-json-ok", 0),
        ]:
            with self.subTest(case=label):
                r = observe(label, GATEWAYS[base], max_retries=0, model=model, **BASE)
                self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, upstream))


class Cancellation(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_client_timeout_closes_the_provider_exchange_and_the_delivered_request_stays_delivered(self) -> None:
        with self.assertRaises(openai.APITimeoutError):
            client(GATEWAYS["standard"], timeout=0.3).responses.create(model="qual-resp-hang", input=f"cancel {secret(9)}", store=False)
        self.assertTrue(provider.await_event(1, "received"), "the provider has the complete sanitized request")
        self.assertTrue(provider.await_event(1, "closed"), "the gateway cancelled the provider exchange")
        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), 1, "no retry or replay after the abort")
        self.assertEqual(json.loads(calls[0]["body"])["input"], "cancel <SECRET_1>", "bytes already delivered stay delivered")

    def test_closing_a_stream_after_its_first_event_closes_the_provider_exchange(self) -> None:
        stream = client(GATEWAYS["standard"]).responses.create(model="qual-resp-sse-hang", input="x", store=False, stream=True)
        next(iter(stream))
        stream.close()
        self.assertTrue(provider.await_event(1, "closed"))
        self.assertEqual(len(provider.calls()["calls"]), 1)

    def test_the_stream_helper_closing_early_closes_the_provider_exchange(self) -> None:
        with client(GATEWAYS["standard"]).responses.stream(model="qual-resp-sse-hang", input="x", store=False) as helper:
            next(iter(helper))
        self.assertTrue(provider.await_event(1, "closed"))

    def test_repeated_cancellation_returns_every_permit(self) -> None:
        for i in range(1, 13):
            with self.assertRaises(openai.APITimeoutError):
                client(GATEWAYS["standard"], timeout=0.3).responses.create(model="qual-resp-hang", input="x", store=False)
            self.assertTrue(provider.await_event(i, "closed"), f"JSON cancel {i} reached the provider as a close")
        base = len(provider.calls()["calls"])
        for i in range(1, 13):
            stream = client(GATEWAYS["standard"]).responses.create(model="qual-resp-sse-hang", input="x", store=False, stream=True)
            next(iter(stream))
            stream.close()
            self.assertTrue(provider.await_event(base + i, "closed"), f"SSE cancel {i} reached the provider as a close")
        ok = client(GATEWAYS["standard"]).responses.create(model="qual-resp-json-ok", input="x", store=False)
        self.assertEqual(ok.output_text, STREAM_TEXT, "capacity fully returned")
        with client(GATEWAYS["standard"]).responses.create(model="qual-resp-sse-ok", input="x", store=False, stream=True) as s:
            self.assertTrue(any(e.type == "response.completed" for e in s))


if __name__ == "__main__":
    unittest.main()
