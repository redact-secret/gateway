"""Responses field matrix and policy outcomes through the pinned OpenAI Python SDK (#88).

One subtest per row of qualification/responses-cases.json (shared with the Node harness). Forwarded
rows are checked against the fake provider's RECORDED request: the parsed body must equal the
expected sanitized body (every retained structure, not just HTTP 200), the planted secrets must be
absent, the placeholder count must match, and exactly one request on exactly one connection must
have been delivered, to /v1/responses, with the provider credential and no local token. Rejected
rows must show the documented status and safe code, no secret-bearing error text, and ZERO provider
connections and requests. A coverage test pins the slot list of the contract matrix.
"""

import inspect
import json
import sys
import unittest
from typing import Any

import openai
from alpha2_support import placeholder_count, planted_in
from qual_support import STREAM_TEXT, assert_nothing_upstream, auth_sha, client, leaks, provider, write_evidence
from responses_support import CASES, GATEWAY_FOR, expected_body, normalize, plain_params, raw_bytes, raw_post_to

ROWS: list[dict[str, Any]] = []

TEXT_SLOTS = [
    "instructions", "input.string", "input.message.content.string", "input.message.content.parts", "function_call.arguments.leaf",
    "function_call_output.output.string", "function_call_output.output.parts", "tools.description", "tools.schema.description", "tools.schema.title",
    "text.format.description", "text.format.schema.description", "text.format.schema.title", "metadata.value",
]
LABEL_SLOTS = [
    "model", "function-call-call-id", "function-call-name", "function-call-output-call-id", "function-call-arguments-key", "tool-name",
    "tool-schema-property-key", "tool-schema-required-entry", "tool-schema-enum-string", "tool-schema-const-string", "tool-choice-name",
    "text-format-name", "text-format-schema-property-key", "text-format-schema-enum-string", "metadata-key",
]
STRUCTURAL = ["role", "message-type", "part-type", "phase", "item-type", "tool-type", "tool-choice-string", "tool-choice-type", "text-format-type", "verbosity", "stream-options-key"]


class ResponsesCases(unittest.TestCase):
    def run_case(self, case: dict[str, Any], gateway: str) -> None:
        label = f"{case['id']}@{gateway}"
        provider.reset()
        base = GATEWAY_FOR[gateway]
        via_sdk = "params" in case
        status: int | None = None
        code: str | None = None
        rendered = ""
        completed = False
        if via_sdk:
            try:
                c = client(base)
                params = plain_params(case)
                known = set(inspect.signature(c.responses.create).parameters)
                kwargs = {k: v for k, v in params.items() if k in known}
                extra = {k: v for k, v in params.items() if k not in known}  # keys the SDK has no keyword for
                out = c.responses.create(extra_body=extra or None, **kwargs)
                if case.get("stream"):
                    with out:
                        completed = any(event.type == "response.completed" for event in out)
                else:
                    completed = out.output_text == STREAM_TEXT
                status = 200
            except openai.APIStatusError as err:
                status, code = err.status_code, err.code
                rendered = json.dumps({"m": err.message, "b": err.body, "h": dict(err.response.headers)}, default=str)
        else:
            status, code, rendered = raw_post_to(base, case.get("endpoint", "responses"), raw_bytes(case))
            completed = status == 200
        seen = provider.calls()
        row = {
            "id": case["id"],
            "group": case["group"],
            "gateway": gateway,
            "path": "sdk" if via_sdk else "raw",
            "outcome": case["expect"]["kind"],
            "status": status,
            "provider_requests": len(seen["calls"]),
            "provider_connections": seen["connections"],
            "placeholders": 0,
        }
        ROWS.append(row)

        expect = case["expect"]
        if expect["kind"] == "reject":
            self.assertEqual(status, expect["status"], f"{label}: status")
            self.assertEqual(code, expect["code"], f"{label}: safe error code")
            self.assertEqual(leaks(rendered), [], f"{label}: error text leaks request content")
            self.assertEqual(planted_in(rendered), [], f"{label}: error text carries a secret marker")
            assert_nothing_upstream(self, label)
            return

        self.assertEqual(status, 200, f"{label}: forwarded")
        self.assertTrue(completed, f"{label}: the provider reply was relayed")
        self.assertEqual(len(seen["calls"]), 1, f"{label}: exactly one upstream request")
        self.assertEqual(seen["connections"], 1, f"{label}: exactly one upstream connection")
        call = seen["calls"][0]
        self.assertEqual(call["method"], "POST")
        self.assertEqual(call["path"], "/v1/responses", f"{label}: the Responses destination, not the Chat one")
        self.assertEqual(call["authorization_sha256"], auth_sha(), f"{label}: the caller's provider credential, unchanged")
        self.assertFalse(call["local_token_seen"])
        sent = json.loads(call["body"])
        want = expected_body(case)
        got, got_arguments = normalize(sent)
        exp, _ = normalize(want)
        self.assertEqual(got, exp, f"{label}: sanitized body differs from the expected one")
        row["placeholders"] = placeholder_count(sent)
        self.assertEqual(row["placeholders"], placeholder_count(want), f"{label}: placeholder count")
        self.assertIs(sent.get("store"), False, f"{label}: store:false is always written")
        if not case.get("plaintext_forwarded"):
            self.assertEqual(planted_in(call["body"]), [], f"{label}: a planted secret reached the provider")
        if "exact_arguments" in case:
            self.assertEqual(got_arguments, case["exact_arguments"], f"{label}: re-encoded arguments strings")

    def test_cases(self) -> None:
        for case in CASES:
            for gateway in case.get("gateways", ["standard"]):
                with self.subTest(case=case["id"], gateway=gateway):
                    self.run_case(case, gateway)

    def test_every_text_slot_has_a_redaction_row(self) -> None:
        for slot in TEXT_SLOTS:
            self.assertTrue(
                any(c["expect"]["kind"] == "forward" and slot in c["slots"] and c["expect"].get("map") for c in CASES),
                f"no redaction row for {slot}",
            )

    def test_every_label_slot_has_a_rejection_row(self) -> None:
        for slot in LABEL_SLOTS:
            self.assertTrue(any(c["expect"]["kind"] == "reject" and f"label.{slot}" in c["slots"] for c in CASES), f"no label row for {slot}")

    def test_every_structural_field_has_a_rejection_row(self) -> None:
        for slot in STRUCTURAL:
            self.assertTrue(any(c["expect"]["kind"] == "reject" and f"structural.{slot}" in c["slots"] for c in CASES), f"no structural row for {slot}")

    @classmethod
    def tearDownClass(cls) -> None:
        write_evidence(
            "responses-cases-python.json",
            {
                "sdk": f"openai (PyPI) {openai.__version__}",
                "python": sys.version.split()[0],
                "cases": len(ROWS),
                "forwarded": sum(1 for r in ROWS if r["outcome"] == "forward"),
                "rejected": sum(1 for r in ROWS if r["outcome"] == "reject"),
                "rejected_with_upstream_traffic": sum(
                    1 for r in ROWS if r["outcome"] == "reject" and (r["provider_requests"] or r["provider_connections"])
                ),
                "rows": ROWS,
            },
        )


if __name__ == "__main__":
    unittest.main()
