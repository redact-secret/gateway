"""One request from the pinned Python OpenAI SDK for header-size measurement (#41, ADR 0023).

Run by header-sizes.mjs, which owns the raw listener. Synthetic credentials only.
"""

import json
import sys

import openai
from openai import OpenAI

base_url, case = sys.argv[1], json.loads(sys.argv[2])
headers = dict(case.get("extra") or {})
if case.get("uaSuffix"):
    headers["User-Agent"] = f"OpenAI/Python {openai.__version__}" + case["uaSuffix"]
client = OpenAI(
    api_key=case["key"],
    base_url=base_url,
    organization=case.get("org"),
    project=case.get("project"),
    max_retries=0,
    default_headers=headers,
)
client.chat.completions.create(model="m", messages=[{"role": "user", "content": "hi"}])
