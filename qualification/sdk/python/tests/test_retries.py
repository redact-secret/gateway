"""OBSERVED retry behavior of the pinned Python SDK (default max_retries = 2) against the gateway.

Every row counts HTTP attempts the SDK made to the gateway (an httpx request hook) and requests
that actually reached the fake provider, then records them as evidence. The assertions encode the
documented table in docs/contracts/errors-and-telemetry.md: if the SDK changes its policy, this
test fails and the docs must be reconciled.
"""

import sys
import time
import unittest
from typing import Any

import openai
from qual_support import (
    DIRECT_PROVIDER,
    GATEWAYS,
    SYN,
    closed_port,
    counting_http_client,
    provider,
    secret,
    write_evidence,
)

ROWS: list[dict[str, Any]] = []
MSG = [{"role": "user", "content": "retry observation"}]


def observe(label: str, base: str, status_label: str, **params: Any) -> dict[str, Any]:
    provider.reset()
    http, attempts = counting_http_client()
    # max_retries left at the SDK default on purpose: this is what an unconfigured app gets.
    c = openai.OpenAI(base_url=f"{base}/v1", api_key=SYN["api_key"], http_client=http)
    extra_body = params.pop("extra_body", None)
    started = time.monotonic()
    outcome = "ok"
    try:
        result = c.chat.completions.create(extra_body=extra_body, **params)
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
        "gateway_or_provider_status": status_label,
        "sdk_outcome": outcome,
        "sdk_attempts": len(attempts),
        "provider_requests": len(seen["calls"]),
        "provider_connections": seen["connections"],
        "elapsed_ms": round((time.monotonic() - started) * 1000),
    }
    ROWS.append(row)
    return row


def tearDownModule() -> None:
    write_evidence(
        "retry-observations-python.json",
        {
            "sdk": "openai (PyPI) 3.24.0",
            "python": sys.version.split()[0],
            "default_max_retries": 2,
            "note": "attempts are HTTP requests the SDK sent to the gateway; provider_requests are those that reached the fake provider",
            "rows": ROWS,
        },
    )


class Retries(unittest.TestCase):
    def test_provider_statuses(self) -> None:
        table = {400: False, 401: False, 403: False, 404: False, 408: True, 409: True, 422: False, 429: True, 500: True, 502: True, 503: True, 504: True}
        for status, retried in table.items():
            with self.subTest(status=status):
                r = observe(f"provider {status}", GATEWAYS["standard"], str(status), model=f"qual-err-{status}", messages=MSG)
                self.assertEqual(r["sdk_attempts"], 3 if retried else 1)
                self.assertEqual(r["provider_requests"], r["sdk_attempts"], "every SDK attempt reached the provider")

    def test_provider_429_with_retry_after_zero_is_retried(self) -> None:
        r = observe("provider 429 + Retry-After 0", GATEWAYS["standard"], "429", model="qual-err-429-retry-after", messages=MSG)
        self.assertEqual(r["sdk_attempts"], 3)

    def test_provider_retry_hints_are_dropped_by_the_gateway(self) -> None:
        # Control: the SDK obeys `x-should-retry: false` when it sees it (direct to the provider) ...
        direct = observe(
            "provider 500 + x-should-retry false (direct, no gateway)",
            DIRECT_PROVIDER,
            "500",
            model="qual-err-500-hint-no-retry",
            messages=MSG,
        )
        self.assertEqual(direct["sdk_attempts"], 1)
        # ... but the gateway relays only allowlisted headers, so through it the hint never arrives
        # and the SDK falls back to its status-based policy (a 5xx is retried).
        via = observe(
            "provider 500 + x-should-retry false (through the gateway)",
            GATEWAYS["standard"],
            "500",
            model="qual-err-500-hint-no-retry",
            messages=MSG,
        )
        self.assertEqual(via["sdk_attempts"], 3)

    def test_two_429s_then_success(self) -> None:
        r = observe("provider 429, 429, then 200", GATEWAYS["standard"], "429,429,200", model="qual-retry-429-then-ok", messages=MSG)
        self.assertEqual(r["sdk_outcome"], "ok")
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))

    def test_gateway_generated_errors(self) -> None:
        r = observe("gateway 422 unsupported_input", GATEWAYS["standard"], "422", model="qual-json-ok", messages=MSG, extra_body={"frobnicate": 1})
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 0))
        r = observe(
            "gateway 413 limit_exceeded",
            GATEWAYS["tight"],
            "413",
            model="qual-json-ok",
            messages=[{"role": "user", "content": f"{secret(51)} {secret(52)} {secret(53)}"}],
        )
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 0))
        r = observe("gateway 501 not_implemented", GATEWAYS["no_upstream"], "501", model="qual-json-ok", messages=MSG)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))
        r = observe("gateway 502 upstream_unavailable", GATEWAYS["dead_provider"], "502", model="qual-json-ok", messages=MSG)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))
        r = observe("gateway 502 upstream_response_too_large", GATEWAYS["standard"], "502", model="qual-json-oversize", messages=MSG)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))
        r = observe("gateway 502 upstream_invalid_response", GATEWAYS["standard"], "502", model="qual-json-truncated", messages=MSG)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))
        r = observe("gateway 504 upstream_timeout", GATEWAYS["tight"], "504", model="qual-hang", messages=MSG)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))

    def test_gateway_overload_is_retried_after_the_relayed_wait_and_never_forwarded(self) -> None:
        provider.reset()
        holder = openai.OpenAI(base_url=f"{GATEWAYS['overload']}/v1", api_key=SYN["api_key"], max_retries=0)
        # Hold the only upstream and stream permit with a stalled stream, then ask for another.
        stream = holder.chat.completions.create(model="qual-sse-hang", messages=MSG, stream=True)
        iterator = iter(stream)
        next(iterator)
        r = observe("gateway 503 overload", GATEWAYS["overload"], "503", model="qual-json-ok", messages=MSG)
        stream.close()
        self.assertEqual(r["sdk_attempts"], 3)
        self.assertEqual(r["provider_requests"], 0, "nothing was sent for the refused requests")
        self.assertGreaterEqual(r["elapsed_ms"], 1900, "the SDK honored Retry-After: 1 twice")

    def test_connection_error_to_the_gateway_is_retried(self) -> None:
        r = observe("connection refused (gateway down)", f"http://127.0.0.1:{closed_port()}", "none", model="qual-json-ok", messages=MSG)
        self.assertEqual(r["sdk_outcome"], "error APIConnectionError")
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 0))

    def test_streams(self) -> None:
        r = observe("SSE interrupted after headers", GATEWAYS["standard"], "200 then cut", model="qual-sse-interrupted", messages=MSG, stream=True)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (1, 1))
        r = observe("SSE provider 429 before headers", GATEWAYS["standard"], "429", model="qual-sse-err-429", messages=MSG, stream=True)
        self.assertEqual((r["sdk_attempts"], r["provider_requests"]), (3, 3))


if __name__ == "__main__":
    unittest.main()
