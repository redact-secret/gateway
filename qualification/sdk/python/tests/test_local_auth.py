"""Caller and provider credentials are separate authorities (#65, docs/contracts/local-caller-auth.md),
seen through the pinned Python SDK, raw HTTP for the shapes an SDK cannot emit, and the fake
provider's own record as the oracle: the provider must never receive the local token, and a
rejection delivers no upstream connection and no request body.
"""

import json
import re
import socket
import unittest
from concurrent.futures import ThreadPoolExecutor
from typing import Any
from urllib.parse import urlparse

import openai
from qual_support import GATEWAYS, SYN, assert_nothing_upstream, auth_sha, client, leaks, provider, secret, write_evidence

LOCAL = "X-Gateway-Local-Token"
TOKEN: str = SYN["local_token"]
DECOY: str = SYN["local_token_decoy"]
PROVIDER_HEADER = f"Authorization: Bearer {SYN['api_key']}"
EVIDENCE: list[dict[str, Any]] = []

# Two instances with different static content policy and different token delivery.
INSTANCES = [("auth_file", "file", True), ("auth_env", "env", False)]


def body(content: str = "hello") -> str:
    return json.dumps({"model": "qual-json-ok", "messages": [{"role": "user", "content": content}]})


def raw_post(base: str, header_lines: list[str], payload: str, declared: int | None = None) -> tuple[int, str]:
    """One raw HTTP/1.1 POST; returns (status, body) even if the server resets after replying."""
    parsed = urlparse(base)
    length = len(payload.encode()) if declared is None else declared
    head = "\r\n".join(
        [
            "POST /v1/chat/completions HTTP/1.1",
            f"Host: {parsed.hostname}:{parsed.port}",
            "Content-Type: application/json",
            f"Content-Length: {length}",
            "Connection: close",
            *header_lines,
            "",
            "",
        ]
    )
    chunks: list[bytes] = []
    with socket.create_connection((parsed.hostname, parsed.port), timeout=10) as s:
        s.sendall(head.encode() + payload.encode())
        try:
            while True:
                data = s.recv(65536)
                if not data:
                    break
                chunks.append(data)
        except OSError:
            pass
    raw = b"".join(chunks).decode("utf-8", "replace")
    match = re.match(r"HTTP/1\.1 (\d{3})", raw)
    if not match:
        raise AssertionError("no HTTP response")
    return int(match.group(1)), raw.split("\r\n\r\n", 1)[1]


def make_class(gateway: str, delivery: str, redacts: bool) -> type:
    base = GATEWAYS[gateway]

    class LocalAuth(unittest.TestCase):
        def setUp(self) -> None:
            provider.reset()

        def with_token(self, **kwargs: Any) -> Any:
            return client(base, default_headers={LOCAL: TOKEN}, **kwargs)

        def test_both_credentials_the_provider_gets_the_provider_key_and_no_trace_of_the_local_token(self) -> None:
            reply = self.with_token().chat.completions.create(
                model="qual-json-ok", messages=[{"role": "user", "content": f"token {secret(1)}"}]
            )
            self.assertEqual(reply.choices[0].finish_reason, "stop")
            calls = provider.calls()["calls"]
            self.assertEqual(len(calls), 1)
            call = calls[0]
            self.assertEqual(call["authorization_sha256"], auth_sha(), "provider key forwarded unchanged")
            self.assertNotIn(LOCAL.lower(), call["header_names"])
            self.assertFalse(call["local_token_seen"], "local token (or decoy) reached the provider")
            self.assertNotIn(TOKEN, call["body"])
            if redacts:
                self.assertIn("<SECRET_1>", call["body"])
                self.assertNotIn("ghp_", call["body"])
            EVIDENCE.append({"gateway": gateway, "check": "both credentials", "status": 200, "upstream_calls": 1, "local_token_seen": False})

        def test_a_streamed_request_carries_the_same_separation(self) -> None:
            finish = None
            with self.with_token().chat.completions.create(
                model="qual-example", messages=[{"role": "user", "content": "hello"}], stream=True
            ) as stream:
                for chunk in stream:
                    if chunk.choices:
                        finish = chunk.choices[0].finish_reason or finish
            self.assertEqual(finish, "stop")
            calls = provider.calls()["calls"]
            self.assertEqual(len(calls), 1)
            self.assertFalse(calls[0]["local_token_seen"])
            self.assertNotIn(LOCAL.lower(), calls[0]["header_names"])

        def test_sdk_rejections_carry_a_safe_code_and_send_nothing_upstream(self) -> None:
            cases = [
                ("no local token", client(base), "local_auth_required"),
                ("the provider key sent as the local token", client(base, default_headers={LOCAL: SYN["api_key"]}), "local_auth_invalid"),
                ("a different well-formed token", client(base, default_headers={LOCAL: DECOY}), "local_auth_invalid"),
                # Authorization is never read for local authentication.
                ("the local token sent only as the provider key", client(base, api_key=TOKEN), "local_auth_required"),
            ]
            for why, sdk, code in cases:
                with self.subTest(why):
                    provider.reset()
                    planted = f"{secret(2)} -----BEGIN PRIVATE KEY----- hunter2xyz"
                    with self.assertRaises(openai.APIStatusError, msg=why) as caught:
                        sdk.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": planted}])
                    err = caught.exception
                    self.assertEqual(err.status_code, 401, why)
                    self.assertEqual(err.code, code, why)
                    rendered = json.dumps({"m": err.message, "b": err.body, "h": dict(err.response.headers)})
                    self.assertEqual(leaks(rendered), [], f"{why}: SDK-visible error exposes a secret value")
                    assert_nothing_upstream(self, why)
                    EVIDENCE.append({"gateway": gateway, "check": why, "status": err.status_code, "code": err.code, "upstream_connections": 0})

        def test_a_valid_local_token_without_a_provider_authorization_is_missing_credential(self) -> None:
            status, text = raw_post(base, [f"{LOCAL}: {TOKEN}"], body())
            self.assertEqual((status, text), (401, '{"error":{"code":"missing_credential"}}'))
            assert_nothing_upstream(self, "missing provider credential")

        def test_malformed_local_headers_are_local_auth_invalid_with_the_fixed_body(self) -> None:
            cases = {
                "a duplicated header (both valid)": [f"{LOCAL}: {TOKEN}", f"{LOCAL}: {TOKEN}"],
                "a Bearer prefix": [f"{LOCAL}: Bearer {TOKEN}"],
                "a comma-joined list": [f"{LOCAL}: {TOKEN},{DECOY}"],
                "an embedded space": [f"{LOCAL}: {TOKEN[:20]} {TOKEN[20:]}"],
                "an empty value": [f"{LOCAL}:"],
                "31 bytes": [f"{LOCAL}: {'a' * 31}"],
                "129 bytes": [f"{LOCAL}: {'a' * 129}"],
                "a quoted value": [f'{LOCAL}: "{TOKEN}"'],
                "characters outside the alphabet": [f"{LOCAL}: {TOKEN[:40]}+/=abc"],
            }
            for why, lines in cases.items():
                with self.subTest(why):
                    provider.reset()
                    status, text = raw_post(base, [PROVIDER_HEADER, *lines], body(f"secret {secret(3)}"))
                    self.assertEqual(status, 401, why)
                    self.assertEqual(text, '{"error":{"code":"local_auth_invalid"}}', why)
                    self.assertEqual(leaks(text), [])
                    assert_nothing_upstream(self, why)

        def test_a_header_nominated_by_connection_is_removed(self) -> None:
            status, text = raw_post(base, [PROVIDER_HEADER, f"{LOCAL}: {TOKEN}", "Connection: x-gateway-local-token"], body())
            self.assertEqual((status, text), (401, '{"error":{"code":"local_auth_required"}}'))
            assert_nothing_upstream(self, "hop-by-hop removal")

        def test_header_name_is_case_insensitive_and_never_forwarded(self) -> None:
            status, _ = raw_post(base, [PROVIDER_HEADER, f"x-gateway-local-token: {TOKEN}"], body())
            self.assertEqual(status, 200)
            calls = provider.calls()["calls"]
            self.assertEqual(len(calls), 1)
            self.assertFalse(calls[0]["local_token_seen"])

        def test_health_and_readiness_never_need_the_token_and_ignore_a_wrong_one(self) -> None:
            import urllib.request

            for route in ("/healthz", "/readyz"):
                for headers in ({}, {LOCAL: DECOY}):
                    request = urllib.request.Request(f"{base}{route}", headers=headers)
                    with urllib.request.urlopen(request, timeout=10) as response:  # noqa: S310 - loopback
                        self.assertEqual(response.status, 200, route)
                        self.assertEqual(leaks(response.read().decode()), [])
            assert_nothing_upstream(self, "health")

        def test_bounded_unauthenticated_load_every_attempt_is_401_and_the_gateway_still_serves(self) -> None:
            total, declared = 240, 4 * 1024 * 1024  # an announced body that is never sent

            def attempt(i: int) -> tuple[int, str]:
                lines = [PROVIDER_HEADER] if i % 2 == 0 else [PROVIDER_HEADER, f"{LOCAL}: {DECOY}"]
                return raw_post(base, lines, body(f"secret {secret(4)}"), declared)

            with ThreadPoolExecutor(max_workers=40) as pool:
                results = list(pool.map(attempt, range(total)))
            self.assertEqual(len(results), total)
            self.assertEqual(
                sorted({f"{s} {t}" for s, t in results}),
                ['401 {"error":{"code":"local_auth_invalid"}}', '401 {"error":{"code":"local_auth_required"}}'],
            )
            assert_nothing_upstream(self, "unauthenticated load")
            ok = self.with_token().chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": "after the load"}])
            self.assertEqual(ok.choices[0].finish_reason, "stop")
            EVIDENCE.append({"gateway": gateway, "check": "unauthenticated load", "attempts": total, "upstream_connections": 0, "still_serves": True})

    LocalAuth.__name__ = f"LocalAuth_{gateway}_{delivery}"
    LocalAuth.__qualname__ = LocalAuth.__name__
    return LocalAuth


for _gateway, _delivery, _redacts in INSTANCES:
    _cls = make_class(_gateway, _delivery, _redacts)
    globals()[_cls.__name__] = _cls


class ZZEvidence(unittest.TestCase):
    def test_records_no_token_value(self) -> None:
        import platform

        write_evidence("local-auth-python.json", {"runtime": platform.python_version(), "rows": EVIDENCE})
        self.assertEqual(leaks(json.dumps(EVIDENCE)), [])


if __name__ == "__main__":
    unittest.main()
