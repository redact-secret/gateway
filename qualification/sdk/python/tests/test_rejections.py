"""Rejected inputs never deliver upstream body bytes, seen through the pinned Python SDK.

"Never" is asserted on the fake provider's own record: zero TCP connections and zero requests.
"""

import json
import unittest
from typing import Any

import openai
from qual_support import GATEWAYS, assert_nothing_upstream, client, leaks, prompt_marker, provider, secret

BASE: dict[str, Any] = {"model": "qual-json-ok", "messages": [{"role": "user", "content": "hello"}]}


class Rejections(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def rejected(self, gateway: str, status: int, code: str, why: str, *, extra_body: Any = None, **params: Any) -> None:
        provider.reset()
        with self.assertRaises(openai.APIStatusError, msg=why) as caught:
            client(GATEWAYS[gateway]).chat.completions.create(extra_body=extra_body, **params)
        err = caught.exception
        self.assertEqual(err.status_code, status, f"{why}: status")
        self.assertEqual(err.code, code, f"{why}: safe error code")
        rendered = json.dumps({"m": err.message, "b": err.body, "h": dict(err.response.headers)})
        self.assertEqual(leaks(rendered), [], f"{why}: error leaks request text")
        assert_nothing_upstream(self, why)

    def test_unsupported_or_unknown_inputs_are_422_unsupported_input(self) -> None:
        marker = prompt_marker()
        text_part = {"type": "text", "text": "hi", "cache_control": 1}
        cases: list[tuple[str, dict[str, Any], Any]] = [
            ("unknown top-level field", dict(BASE), {"frobnicate": 1}),
            ("tools", {**BASE, "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]}, None),
            ("tool_choice", {**BASE, "tool_choice": "auto"}, None),
            (
                "image_url content part",
                {"model": "qual-json-ok", "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "https://example.invalid/x.png"}}]}]},
                None,
            ),
            ("unknown field inside a text part", {"model": "qual-json-ok", "messages": [{"role": "user", "content": [text_part]}]}, None),
            ("metadata", {**BASE, "metadata": {"k": "v"}}, None),
            ("logprobs", {**BASE, "logprobs": True}, None),
            ("participant name on a message", {"model": "qual-json-ok", "messages": [{"role": "user", "name": "alice", "content": "hi"}]}, None),
            ("tool role message", {"model": "qual-json-ok", "messages": [{"role": "tool", "tool_call_id": "x", "content": "hi"}]}, None),
            ("null content", {"model": "qual-json-ok", "messages": [{"role": "assistant", "content": None}]}, None),
            ("n greater than one", {**BASE, "n": 2}, None),
            (
                "response_format json_schema",
                {**BASE, "response_format": {"type": "json_schema", "json_schema": {"name": "s", "schema": {}}}},
                None,
            ),
            (
                "secret hidden next to an unknown field",
                {"model": "qual-json-ok", "messages": [{"role": "user", "content": f"secret {secret(10)} {marker}"}]},
                {"frobnicate": 1},
            ),
            ("stream request with an unknown field", {**BASE, "stream": True}, {"frobnicate": 1}),
        ]
        for name, params, extra in cases:
            with self.subTest(name):
                self.rejected("standard", 422, "unsupported_input", name, extra_body=extra, **params)

    def test_core_completeness_and_policy_failures_never_forward(self) -> None:
        self.rejected("standard", 422, "unsupported_input", "secret in model", model=secret(11), messages=BASE["messages"])
        self.rejected(
            "standard",
            422,
            "unsupported_input",
            "warn finding",
            model="qual-json-ok",
            messages=[{"role": "user", "content": "password=hunter2xyz"}],
        )
        content = " and ".join(secret(n) for n in (21, 22, 23))
        self.rejected(
            "tight", 413, "limit_exceeded", "finding limit", model="qual-json-ok", messages=[{"role": "user", "content": content}]
        )

    def test_over_limit_requests_never_forward(self) -> None:
        self.rejected(
            "tight", 413, "limit_exceeded", "oversize body", model="qual-json-ok", messages=[{"role": "user", "content": "x" * 8192}]
        )
        self.rejected(
            "tight",
            413,
            "limit_exceeded",
            "too many messages",
            model="qual-json-ok",
            messages=[{"role": "user", "content": "hi"}] * 5,
        )

    def test_missing_credential_is_401(self) -> None:
        provider.reset()
        with self.assertRaises(openai.AuthenticationError) as caught:
            client(GATEWAYS["standard"]).chat.completions.create(
                extra_headers={"Authorization": openai.Omit()}, **BASE
            )
        self.assertEqual(caught.exception.code, "missing_credential")
        assert_nothing_upstream(self, "missing credential")

    def test_no_upstream_configured_is_501(self) -> None:
        provider.reset()
        planted = [{"role": "user", "content": f"x {secret(31)}"}]
        for stream in (False, True):
            with self.assertRaises(openai.APIStatusError) as caught:
                client(GATEWAYS["no_upstream"]).chat.completions.create(model="qual-json-ok", messages=planted, stream=stream)
            self.assertEqual(caught.exception.status_code, 501)
            self.assertEqual(caught.exception.code, "not_implemented")
        assert_nothing_upstream(self, "no upstream configured")


if __name__ == "__main__":
    unittest.main()
