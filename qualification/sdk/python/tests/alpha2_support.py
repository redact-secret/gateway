"""Alpha 2 qualification helpers (#57), Python. Synthetic data only.

The case table (qualification/alpha2-cases.json) is shared with the Node harness. The expected
sanitized body is derived from the SAME parameters by replacing each secret token with the
placeholder the contract numbers it to, so a mismatch in any retained structure is a failure.
"""

from __future__ import annotations

import copy
import json
import os
import re
import urllib.error
import urllib.request
from typing import Any, Callable

from qual_support import GATEWAYS, SYN, secret

_cases_path = os.path.join(os.path.dirname(os.environ["QUAL_SYNTHETIC"]), "alpha2-cases.json")
with open(_cases_path, encoding="utf-8") as _f:
    _loaded = json.load(_f)
CASES: list[dict[str, Any]] = _loaded["cases"]
TOKENS: dict[str, str] = _loaded["tokens"]

GATEWAY_FOR = {
    "standard": GATEWAYS["standard"],
    "forward": GATEWAYS["policy_forward"],
    "common": GATEWAYS["policy_common"],
    "tight": GATEWAYS["tight"],
    "concurrent": GATEWAYS["concurrent"],
}

_TOKEN = re.compile(r"\{\{(E?)(S\d+|PEM|WARN)\}\}")


def _escape_all(text: str) -> str:
    out = []
    for ch in text:
        cp = ord(ch)
        if cp > 0xFFFF:  # surrogate pair, as JSON writers do
            cp -= 0x10000
            out.append(f"\\u{0xD800 + (cp >> 10):04x}\\u{0xDC00 + (cp & 0x3FF):04x}")
        else:
            out.append(f"\\u{cp:04x}")
    return "".join(out)


def substitute(text: str, placeholders: dict[str, int] | None) -> str:
    def repl(m: re.Match[str]) -> str:
        esc, name = m.group(1), m.group(2)
        if name in ("PEM", "WARN"):
            return TOKENS[name]
        if placeholders is not None and name in placeholders:
            return f"<SECRET_{placeholders[name]}>"
        plain = secret(int(name[1:]))
        return _escape_all(plain) if esc else plain

    return _TOKEN.sub(repl, text)


def map_deep(value: Any, f: Callable[[str], str]) -> Any:
    if isinstance(value, str):
        return f(value)
    if isinstance(value, list):
        return [map_deep(v, f) for v in value]
    if isinstance(value, dict):
        return {f(k): map_deep(v, f) for k, v in value.items()}
    return value


def normalize(body: Any) -> tuple[Any, list[str]]:
    """Parse every tool-call `arguments` string so trees compare structurally; list the raw strings."""
    argument_strings: list[str] = []
    copied = copy.deepcopy(body)
    for message in copied.get("messages", []):
        for tool_call in message.get("tool_calls", []) or []:
            raw = tool_call["function"]["arguments"]
            argument_strings.append(raw)
            try:
                tool_call["function"]["arguments"] = json.loads(raw)
            except ValueError:
                pass
    return copied, argument_strings


def plain_params(case: dict[str, Any]) -> dict[str, Any]:
    return map_deep(case["params"], lambda s: substitute(s, None))


def expected_body(case: dict[str, Any]) -> Any:
    if "body" in case["expect"]:
        return case["expect"]["body"]
    placeholders = case["expect"].get("map", {})
    return map_deep(case["params"], lambda s: substitute(s, placeholders))


def raw_bytes(case: dict[str, Any]) -> bytes:
    if "raw_hex" in case:
        return bytes.fromhex(case["raw_hex"])
    return substitute(case["raw"], None).encode("utf-8")


def raw_post(base: str, data: bytes) -> tuple[int, str | None, str]:
    """A hand-built POST (no SDK), for bodies an SDK cannot produce: duplicate keys, bad UTF-8."""
    request = urllib.request.Request(
        f"{base}/v1/chat/completions",
        data=data,
        method="POST",
        headers={"content-type": "application/json", "authorization": f"Bearer {SYN['api_key']}"},
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


def planted_in(text: str) -> list[str]:
    return [m for m in ("ghp_SYNTH", "BEGIN PRIVATE KEY", "U1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ") if m in text]


def placeholder_count(value: Any) -> int:
    return json.dumps(value, ensure_ascii=False).count("<SECRET_")
