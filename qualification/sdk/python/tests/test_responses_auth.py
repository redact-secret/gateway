"""Caller and provider credentials are independent authorities on BOTH endpoints (#88, extending #65).

The same two authenticated instances as test_local_auth.py (`auth_file`: token file, full profile;
`auth_env`: token environment variable, common profile with on_warn forward) are driven through
POST /v1/responses with the pinned SDK and raw HTTP, and through Chat and Responses interleaved on
one instance. Oracle: the fake provider's own record (`local_token_seen`, header names, the
Authorization hash, connections and requests).
"""

import json
import platform
import re
import socket
import unittest
from concurrent.futures import ThreadPoolExecutor
from typing import Any
from urllib.parse import urlparse

import openai
from qual_support import GATEWAYS, STREAM_TEXT, SYN, assert_nothing_upstream, auth_sha, client, leaks, provider, secret, write_evidence

LOCAL = "X-Gateway-Local-Token"
TOKEN: str = SYN["local_token"]
DECOY: str = SYN["local_token_decoy"]
PROVIDER_HEADER = f"Authorization: Bearer {SYN['api_key']}"
EVIDENCE: list[dict[str, Any]] = []
R = "/v1/responses"
C = "/v1/chat/completions"
INSTANCES = [("auth_file", True), ("auth_env", False)]


def resp_body(content: str = "hello") -> str:
    return json.dumps({"model": "qual-resp-json-ok", "input": content, "store": False})


def chat_body(content: str = "hello") -> str:
    return json.dumps({"model": "qual-json-ok", "messages": [{"role": "user", "content": content}]})


def raw_post(base: str, target: str, header_lines: list[str], payload: str, declared: int | None = None) -> tuple[int, str]:
    """One raw HTTP/1.1 POST; returns (status, body) even if the server resets after replying."""
    parsed = urlparse(base)
    length = len(payload.encode()) if declared is None else declared
    head = "\r\n".join(
        [f"POST {target} HTTP/1.1", f"Host: {parsed.hostname}:{parsed.port}", "Content-Type: application/json", f"Content-Length: {length}", "Connection: close", *header_lines, "", ""]
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


def make_class(gateway: str, redacts: bool) -> type:
    base = GATEWAYS[gateway]

    class ResponsesAuth(unittest.TestCase):
        def setUp(self) -> None:
            provider.reset()

        def with_token(self) -> Any:
            return client(base, default_headers={LOCAL: TOKEN})

        def test_both_credentials_provider_gets_its_key_on_the_responses_path_and_no_trace_of_the_local_token(self) -> None:
            reply = self.with_token().responses.create(model="qual-resp-json-ok", input=f"token {secret(1)}", instructions="be brief", store=False)
            self.assertEqual(reply.output_text, STREAM_TEXT)
            seen = provider.calls()
            self.assertEqual((len(seen["calls"]), seen["connections"]), (1, 1))
            call = seen["calls"][0]
            self.assertEqual(call["path"], R)
            self.assertEqual(call["authorization_sha256"], auth_sha(), "provider key forwarded unchanged")
            self.assertNotIn(LOCAL.lower(), call["header_names"])
            self.assertFalse(call["local_token_seen"])
            expected = "token <SECRET_1>" if redacts else f"token {secret(1)}"
            self.assertEqual(json.loads(call["body"])["input"], expected)
            EVIDENCE.append({"gateway": gateway, "endpoint": "responses", "check": "both credentials", "status": 200, "upstream_calls": 1, "local_token_seen": False})

        def test_a_streamed_responses_request_carries_the_same_separation(self) -> None:
            with self.with_token().responses.create(model="qual-resp-sse-ok", input="hello", store=False, stream=True) as stream:
                self.assertTrue(any(e.type == "response.completed" for e in stream))
            calls = provider.calls()["calls"]
            self.assertEqual(len(calls), 1)
            self.assertFalse(calls[0]["local_token_seen"])
            self.assertEqual(calls[0]["path"], R)

        def test_one_token_and_one_provider_key_serve_both_endpoints(self) -> None:
            c = self.with_token()
            chat = c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": f"chat {secret(2)}"}])
            resp = c.responses.create(model="qual-resp-json-ok", input=f"resp {secret(3)}", store=False)
            self.assertEqual(chat.choices[0].message.content, STREAM_TEXT)
            self.assertEqual(resp.output_text, STREAM_TEXT)
            seen = provider.calls()
            self.assertEqual([x["path"] for x in seen["calls"]], [C, R])
            self.assertEqual(seen["connections"], 2)
            for call in seen["calls"]:
                self.assertEqual(call["authorization_sha256"], auth_sha())
                self.assertFalse(call["local_token_seen"])
                self.assertNotIn(LOCAL.lower(), call["header_names"])

        def test_sdk_rejections_on_responses_carry_a_safe_code_and_send_nothing_upstream(self) -> None:
            cases = [
                ("no local token", client(base), "local_auth_required"),
                ("the provider key sent as the local token", client(base, default_headers={LOCAL: SYN["api_key"]}), "local_auth_invalid"),
                ("a different well-formed token", client(base, default_headers={LOCAL: DECOY}), "local_auth_invalid"),
                ("the local token sent only as the provider key", client(base, api_key=TOKEN), "local_auth_required"),
            ]
            for why, sdk, code in cases:
                with self.subTest(why):
                    provider.reset()
                    planted = f"{secret(2)} -----BEGIN PRIVATE KEY----- hunter2xyz"
                    with self.assertRaises(openai.APIStatusError, msg=why) as caught:
                        sdk.responses.create(model="qual-resp-json-ok", input=planted, store=False)
                    err = caught.exception
                    self.assertEqual((err.status_code, err.code), (401, code), why)
                    self.assertEqual(leaks(json.dumps({"m": err.message, "b": err.body, "h": dict(err.response.headers)})), [], why)
                    assert_nothing_upstream(self, why)
                    EVIDENCE.append({"gateway": gateway, "endpoint": "responses", "check": why, "status": 401, "code": code, "upstream_connections": 0})

        def test_missing_provider_authorization_is_missing_credential(self) -> None:
            self.assertEqual(raw_post(base, R, [f"{LOCAL}: {TOKEN}"], resp_body()), (401, '{"error":{"code":"missing_credential"}}'))
            assert_nothing_upstream(self, "missing provider credential")

        def test_an_authenticated_invalid_request_is_refused_by_the_protocol_layer(self) -> None:
            with self.assertRaises(openai.APIStatusError) as caught:
                self.with_token().responses.create(model="qual-resp-json-ok", input="x")
            self.assertEqual((caught.exception.status_code, caught.exception.code), (422, "unsupported_input"))
            assert_nothing_upstream(self, "store missing")

        def test_malformed_local_headers_are_local_auth_invalid_with_the_fixed_body(self) -> None:
            cases = {
                "a duplicated header (both valid)": [f"{LOCAL}: {TOKEN}", f"{LOCAL}: {TOKEN}"],
                "a Bearer prefix": [f"{LOCAL}: Bearer {TOKEN}"],
                "a comma-joined list": [f"{LOCAL}: {TOKEN},{DECOY}"],
                "an empty value": [f"{LOCAL}:"],
                "31 bytes": [f"{LOCAL}: {'a' * 31}"],
                "129 bytes": [f"{LOCAL}: {'a' * 129}"],
                "characters outside the alphabet": [f"{LOCAL}: {TOKEN[:40]}+/=abc"],
            }
            for why, lines in cases.items():
                with self.subTest(why):
                    provider.reset()
                    self.assertEqual(raw_post(base, R, [PROVIDER_HEADER, *lines], resp_body(f"secret {secret(3)}")), (401, '{"error":{"code":"local_auth_invalid"}}'), why)
                    assert_nothing_upstream(self, why)

        def test_connection_nominated_removal_and_case_insensitive_name(self) -> None:
            self.assertEqual(
                raw_post(base, R, [PROVIDER_HEADER, f"{LOCAL}: {TOKEN}", "Connection: x-gateway-local-token"], resp_body()),
                (401, '{"error":{"code":"local_auth_required"}}'),
            )
            assert_nothing_upstream(self, "hop-by-hop removal")
            status, _ = raw_post(base, R, [PROVIDER_HEADER, f"x-gateway-local-token: {TOKEN}"], resp_body())
            self.assertEqual(status, 200)
            calls = provider.calls()["calls"]
            self.assertEqual(len(calls), 1)
            self.assertFalse(calls[0]["local_token_seen"])

        def test_wrong_route_probes_never_reach_the_provider(self) -> None:
            lines = [PROVIDER_HEADER, f"{LOCAL}: {TOKEN}"]
            self.assertEqual(raw_post(base, C, lines, resp_body())[0], 422)
            self.assertEqual(raw_post(base, R, lines, chat_body())[0], 422)
            for target in ("/v1/responses/", "/v1/responses?x=1", "/V1/responses", "/v1/response"):
                status, _ = raw_post(base, target, lines, resp_body())
                self.assertTrue(400 <= status < 500, f"{target}: {status}")
            assert_nothing_upstream(self, "wrong routes")

        def test_bounded_unauthenticated_load_across_both_endpoints(self) -> None:
            total, declared = 240, 4 * 1024 * 1024  # an announced body that is never sent

            def attempt(i: int) -> tuple[int, str]:
                lines = [PROVIDER_HEADER] if i % 2 == 0 else [PROVIDER_HEADER, f"{LOCAL}: {DECOY}"]
                return raw_post(base, R if i % 4 < 2 else C, lines, resp_body(f"secret {secret(4)}"), declared)

            with ThreadPoolExecutor(max_workers=40) as pool:
                results = list(pool.map(attempt, range(total)))
            self.assertEqual(len(results), total)
            self.assertEqual(sorted({f"{s} {t}" for s, t in results}), ['401 {"error":{"code":"local_auth_invalid"}}', '401 {"error":{"code":"local_auth_required"}}'])
            assert_nothing_upstream(self, "unauthenticated load")
            c = self.with_token()
            self.assertEqual(c.responses.create(model="qual-resp-json-ok", input="after", store=False).status, "completed")
            self.assertEqual(c.chat.completions.create(model="qual-json-ok", messages=[{"role": "user", "content": "after"}]).choices[0].finish_reason, "stop")
            EVIDENCE.append({"gateway": gateway, "endpoint": "both", "check": "unauthenticated load", "attempts": total, "upstream_connections": 0, "still_serves": True})

    ResponsesAuth.__name__ = f"ResponsesAuth_{gateway}"
    ResponsesAuth.__qualname__ = ResponsesAuth.__name__
    return ResponsesAuth


for _gateway, _redacts in INSTANCES:
    _cls = make_class(_gateway, _redacts)
    globals()[_cls.__name__] = _cls


class ZZEvidence(unittest.TestCase):
    def test_records_no_token_value(self) -> None:
        write_evidence("responses-auth-python.json", {"runtime": platform.python_version(), "rows": EVIDENCE})
        self.assertEqual(leaks(json.dumps(EVIDENCE)), [])


if __name__ == "__main__":
    unittest.main()
