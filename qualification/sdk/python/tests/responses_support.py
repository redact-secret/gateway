"""Responses (`POST /v1/responses`) qualification helpers (#88), Python. Synthetic data only.

The case table (qualification/responses-cases.json) is shared with the Node harness. The expected
sanitized body is derived from the SAME parameters by replacing each secret token with the
placeholder the contract numbers it to, so a mismatch in any retained structure is a failure.
"""

from __future__ import annotations

import copy
import json
import os
import urllib.error
import urllib.request
from typing import Any

from alpha2_support import GATEWAY_FOR, map_deep, substitute
from qual_support import SYN

_path = os.path.join(os.path.dirname(os.environ["QUAL_SYNTHETIC"]), "responses-cases.json")
with open(_path, encoding="utf-8") as _f:
    CASES: list[dict[str, Any]] = json.load(_f)["cases"]

__all__ = ["CASES", "GATEWAY_FOR", "expected_body", "normalize", "plain_params", "raw_bytes", "raw_post_to"]


def plain_params(case: dict[str, Any]) -> dict[str, Any]:
    return map_deep(case["params"], lambda s: substitute(s, None))


def expected_body(case: dict[str, Any]) -> Any:
    placeholders = case["expect"].get("map", {})
    if "params" not in case:  # raw rows: the escapes decode first
        return json.loads(substitute(case["raw"], placeholders))
    return map_deep(case["params"], lambda s: substitute(s, placeholders))


def raw_bytes(case: dict[str, Any]) -> bytes:
    if "raw_hex" in case:
        return bytes.fromhex(case["raw_hex"])
    return substitute(case["raw"], None).encode("utf-8")


def normalize(body: Any) -> tuple[Any, list[str]]:
    """Parse every function_call `arguments` string so trees compare structurally; list the raw strings."""
    strings: list[str] = []
    copied = copy.deepcopy(body)
    items = copied.get("input") if isinstance(copied, dict) else None
    if isinstance(items, list):
        for item in items:
            if isinstance(item, dict) and item.get("type") == "function_call" and isinstance(item.get("arguments"), str):
                strings.append(item["arguments"])
                try:
                    item["arguments"] = json.loads(item["arguments"])
                except ValueError:
                    pass
    return copied, strings


def raw_post_to(base: str, endpoint: str, data: bytes, headers: dict[str, str] | None = None) -> tuple[int, str | None, str]:
    """A hand-built POST (no SDK) to the named endpoint, for bodies an SDK cannot produce."""
    target = "/v1/chat/completions" if endpoint == "chat" else "/v1/responses"
    request = urllib.request.Request(
        f"{base}{target}",
        data=data,
        method="POST",
        headers={"content-type": "application/json", "authorization": f"Bearer {SYN['api_key']}", **(headers or {})},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310 - loopback
            status, text = response.status, response.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        status, text = e.code, e.read().decode("utf-8")
    code = None
    try:
        code = json.loads(text).get("error", {}).get("code")
    except ValueError:
        pass
    return status, code, text
