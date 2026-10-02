"""Supported text forms and secret handling through the pinned OpenAI Python SDK (ADR 0020)."""

import json
import unittest

from qual_support import GATEWAYS, STREAM_TEXT, auth_sha, client, prompt_marker, provider, secret

ALLOWED_PROVIDER_HEADERS = {
    "host",
    "authorization",
    "content-type",
    "content-length",
    "accept",
    "accept-encoding",
    "user-agent",
    "connection",
}


class TextForms(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_relays_a_completion_and_forwards_structure_unchanged(self) -> None:
        params = {
            "model": "qual-json-ok",
            "messages": [
                {"role": "system", "content": "You are a synthetic test assistant."},
                {"role": "developer", "content": "Answer briefly."},
                {"role": "user", "content": "Say hello in Korean: 안녕 🙂"},
                {"role": "assistant", "content": "안녕하세요"},
                {"role": "user", "content": "Thanks."},
            ],
            "temperature": 0.2,
            "top_p": 0.9,
            "max_completion_tokens": 64,
            "presence_penalty": 0.1,
            "frequency_penalty": -0.1,
            "seed": 7,
            "stop": ["END", "STOP"],
            "user": "qual-user-1",
            "response_format": {"type": "json_object"},
            "n": 1,
        }
        res = client(GATEWAYS["standard"]).chat.completions.create(**params)
        self.assertEqual(res.choices[0].message.content, STREAM_TEXT)
        self.assertEqual(res.usage.total_tokens, 8)

        seen = provider.calls()
        self.assertEqual(seen["connections"], 1)
        self.assertEqual(len(seen["calls"]), 1)
        call = seen["calls"][0]
        self.assertEqual((call["method"], call["path"]), ("POST", "/v1/chat/completions"))
        self.assertEqual(json.loads(call["body"]), params, "no field added, dropped, or changed")
        self.assertEqual(call["authorization_sha256"], auth_sha(), "caller's provider credential is forwarded")
        for name in call["header_names"]:
            self.assertIn(name, ALLOWED_PROVIDER_HEADERS, f"unexpected header reached the provider: {name}")
        self.assertFalse([n for n in call["header_names"] if n.startswith("x-stainless")])
        self.assertTrue(call["user_agent"].startswith("redact-secret-gateway/"))

    def test_content_parts_array_stays_an_array(self) -> None:
        params = {
            "model": "qual-json-ok",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "first part"},
                        {"type": "text", "text": "second part, 한국어"},
                    ],
                }
            ],
        }
        res = client(GATEWAYS["standard"]).chat.completions.create(**params)
        self.assertEqual(res.choices[0].message.content, STREAM_TEXT)
        self.assertEqual(json.loads(provider.calls()["calls"][0]["body"]), params)

    def test_planted_secrets_never_reach_the_provider_and_structure_is_preserved(self) -> None:
        marker = prompt_marker()
        params = {
            "model": "qual-json-ok",
            "messages": [
                {"role": "system", "content": f"Policy {marker}. Keep answers short."},
                {"role": "user", "content": f"my token is {secret(1)} thanks, 안녕"},
                {"role": "assistant", "content": [{"type": "text", "text": f"earlier I saw {secret(2)}"}]},
                {"role": "user", "content": "and nothing secret here"},
            ],
            "stop": [f"end-{secret(3)}"],
            "max_tokens": 32,
        }
        res = client(GATEWAYS["standard"]).chat.completions.create(**params)
        self.assertEqual(res.choices[0].message.content, STREAM_TEXT)

        body = provider.calls()["calls"][0]["body"]
        for n in (1, 2, 3):
            self.assertNotIn(secret(n), body, f"secret {n} reached the provider")
        self.assertNotIn("ghp_", body)
        self.assertEqual(body.count("<SECRET_"), 3, "one placeholder per distinct planted secret")
        self.assertIn(marker, body, "surrounding non-secret text survives")
        self.assertIn("안녕", body)

        sent = json.loads(body)
        self.assertEqual([m["role"] for m in sent["messages"]], [m["role"] for m in params["messages"]])
        self.assertIsInstance(sent["messages"][2]["content"], list, "parts array stays an array")
        self.assertEqual(len(sent["stop"]), 1)
        self.assertEqual(sent["max_tokens"], 32)
        # Replacing each placeholder by its secret restores the request exactly.
        restored = body
        import re

        for i, placeholder in enumerate(re.findall(r"<SECRET_\d+>", body), start=1):
            restored = restored.replace(placeholder, secret(i), 1)
        self.assertEqual(json.loads(restored), params, "only the secrets changed")


if __name__ == "__main__":
    unittest.main()
