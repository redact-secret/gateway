"""Alpha 2 field coverage and policy outcomes through the pinned OpenAI Python SDK (#57).

One subtest per row of qualification/alpha2-cases.json (shared with the Node harness). Forwarded
rows are checked against the fake provider's RECORDED request: the parsed body must equal the
expected sanitized body (every retained structure, not just HTTP 200), the planted secrets must be
absent, the placeholder count must match, and exactly one request must have been delivered.
Rejected rows must show the documented status and safe code, no secret-bearing error text, and
ZERO provider connections and requests.
"""

import inspect
import json
import sys
import unittest
from typing import Any

import openai
from alpha2_support import (
    CASES,
    GATEWAY_FOR,
    expected_body,
    normalize,
    placeholder_count,
    plain_params,
    planted_in,
    raw_bytes,
    raw_post,
)
from qual_support import STREAM_TEXT, assert_nothing_upstream, client, leaks, provider, write_evidence

ROWS: list[dict[str, Any]] = []


class Alpha2Cases(unittest.TestCase):
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
                known = set(inspect.signature(c.chat.completions.create).parameters)
                kwargs = {k: v for k, v in params.items() if k in known}
                extra = {k: v for k, v in params.items() if k not in known}  # keys the SDK has no keyword for
                res = c.chat.completions.create(extra_body=extra or None, **kwargs)
                completed = res.choices[0].message.content == STREAM_TEXT
                status = 200
            except openai.APIStatusError as err:
                status, code = err.status_code, err.code
                rendered = json.dumps({"m": err.message, "b": err.body, "h": dict(err.response.headers)}, default=str)
        else:
            status, code, rendered = raw_post(base, raw_bytes(case))
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
        call = seen["calls"][0]
        sent = json.loads(call["body"])
        want = expected_body(case)
        got, got_arguments = normalize(sent)
        exp, _ = normalize(want)
        self.assertEqual(got, exp, f"{label}: sanitized body differs from the expected one")
        row["placeholders"] = placeholder_count(sent)
        self.assertEqual(row["placeholders"], placeholder_count(want), f"{label}: placeholder count")
        if not case.get("plaintext_forwarded"):
            self.assertEqual(planted_in(call["body"]), [], f"{label}: a planted secret reached the provider")
        if "exact_arguments" in case:
            self.assertEqual(got_arguments, case["exact_arguments"], f"{label}: re-encoded arguments strings")

    def test_cases(self) -> None:
        for case in CASES:
            for gateway in case.get("gateways", ["standard"]):
                with self.subTest(case=case["id"], gateway=gateway):
                    self.run_case(case, gateway)

    @classmethod
    def tearDownClass(cls) -> None:
        write_evidence(
            "alpha2-cases-python.json",
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
