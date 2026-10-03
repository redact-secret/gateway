"""Shared helpers for the Python SDK qualification (ADR 0020). Synthetic data only."""

from __future__ import annotations

import hashlib
import json
import os
import socket
import urllib.request
import uuid
from typing import Any

import httpx2
import openai
from openai import OpenAI


def _env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise RuntimeError(f"missing environment variable {name} (run via qualification/run-suites.sh)")
    return value


with open(_env("QUAL_SYNTHETIC"), encoding="utf-8") as _f:
    SYN: dict[str, Any] = json.load(_f)

GATEWAYS = {
    "standard": _env("GATEWAY_STANDARD"),
    "tight": _env("GATEWAY_TIGHT"),
    "no_upstream": _env("GATEWAY_NOUPSTREAM"),
    "dead_provider": _env("GATEWAY_DEADPROVIDER"),
    "overload": _env("GATEWAY_OVERLOAD"),
    "policy_forward": _env("GATEWAY_POLICYFORWARD"),
    "policy_common": _env("GATEWAY_POLICYCOMMON"),
    "concurrent": _env("GATEWAY_CONCURRENT"),
    "auth_file": _env("GATEWAY_AUTHFILE"),
    "auth_env": _env("GATEWAY_AUTHENV"),
}
_ADMIN = _env("QUAL_ADMIN")
DIRECT_PROVIDER = _env("QUAL_PROVIDER")  # control path that bypasses the gateway
STREAM_TEXT: str = SYN["stream_text"]


def _admin(path: str, method: str = "GET") -> Any:
    request = urllib.request.Request(f"{_ADMIN}{path}", method=method)
    with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310 - loopback admin API
        return json.loads(response.read())


class provider:  # noqa: N801 - namespace, mirrors the Node helper
    @staticmethod
    def reset() -> None:
        _admin("/__admin/reset", "POST")

    @staticmethod
    def calls() -> dict[str, Any]:
        return _admin("/__admin/calls")

    @staticmethod
    def await_event(seq: int, event: str, timeout_ms: int = 10_000) -> bool:
        """Event-driven wait on the fake provider; False only when the deadline passes."""
        try:
            return bool(_admin(f"/__admin/await?call={seq}&event={event}&timeout_ms={timeout_ms}")["ok"])
        except urllib.error.HTTPError:
            return False

    @staticmethod
    def release(seq: int) -> None:
        _admin(f"/__admin/release?call={seq}", "POST")


def client(base: str, **kwargs: Any) -> OpenAI:
    kwargs.setdefault("max_retries", 0)
    kwargs.setdefault("api_key", SYN["api_key"])
    return OpenAI(base_url=f"{base}/v1", **kwargs)


def counting_http_client() -> tuple[httpx2.Client, list[int]]:
    """An HTTP client that counts every request the SDK sends (retries included)."""
    attempts: list[int] = []
    http = httpx2.Client(event_hooks={"request": [lambda _request: attempts.append(1)]})
    return http, attempts


def secret(n: int) -> str:
    """A synthetic token the pinned core detects (`full` profile). Never a real credential."""
    return f"{SYN['secret_prefix']}{n:020d}"


def prompt_marker() -> str:
    return f"{SYN['prompt_marker_prefix']}{uuid.uuid4()}"


def auth_sha() -> str:
    return hashlib.sha256(f"Bearer {SYN['api_key']}".encode()).hexdigest()


def leaks(text: str) -> list[str]:
    return [m for m in (SYN["local_token"], SYN["local_token_decoy"], SYN["secret_prefix"], "ghp_SYNTH", SYN["prompt_marker_prefix"], SYN["api_key"], "hunter2xyz", "U1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ") if m in text]


def assert_nothing_upstream(test: Any, why: str) -> None:
    seen = provider.calls()
    test.assertEqual(seen["connections"], 0, f"{why}: provider saw a connection")
    test.assertEqual(len(seen["calls"]), 0, f"{why}: provider saw a request")


def write_evidence(name: str, data: Any) -> None:
    directory = _env("QUAL_EVIDENCE")
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, name), "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")


def closed_port() -> int:
    """A loopback port with nothing listening (connection refused)."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


__all__ = [
    "GATEWAYS",
    "STREAM_TEXT",
    "DIRECT_PROVIDER",
    "SYN",
    "assert_nothing_upstream",
    "auth_sha",
    "client",
    "closed_port",
    "counting_http_client",
    "leaks",
    "openai",
    "prompt_marker",
    "provider",
    "secret",
    "write_evidence",
]
