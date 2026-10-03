"""Manual tool round trips and the SDK's structured-output and tool helpers against POST /v1/responses (#88).

Every row sends a real SDK request through the gateway and compares the fake provider's RECORDED
request, so "accepted" means the sanitized body reached the provider and "rejected" means zero
connections. The helper table is MEASURED against the pinned `openai` 3.24.0 (with its bundled
`pydantic`); it replaces the type-surface review the contract had.
"""

import datetime
import json
import sys
import unittest
import uuid
from enum import Enum
from typing import Any, Callable, Literal

import openai
from pydantic import BaseModel, Field, ValidationError
from qual_support import GATEWAYS, STREAM_TEXT, assert_nothing_upstream, client, leaks, provider, secret, write_evidence

OBSERVATIONS: list[dict[str, Any]] = []

WEATHER = {
    "type": "function",
    "name": "get_weather",
    "description": "Current weather for a city.",
    "parameters": {
        "type": "object",
        "properties": {"city": {"type": "string"}, "days": {"type": "integer"}},
        "required": ["city", "days"],
        "additionalProperties": False,
    },
    "strict": True,
}


def tearDownModule() -> None:
    write_evidence("responses-tools-observations-python.json", {"sdk": "openai (PyPI) 3.24.0", "python": sys.version.split()[0], "observations": OBSERVATIONS})


def last_body() -> dict[str, Any]:
    calls = provider.calls()["calls"]
    assert calls, "the provider received a request"
    return json.loads(calls[-1]["body"])


class ToolRoundTrips(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def round_trip(self, parallel: bool) -> None:
        c = client(GATEWAYS["concurrent"])
        first = c.responses.create(
            model="qual-resp-tool-calls-parallel" if parallel else "qual-resp-tool-call",
            input=f"Weather for {secret(1)}?",
            store=False,
            tools=[WEATHER],
            tool_choice="auto",
            parallel_tool_calls=parallel,
        )
        calls = [i for i in first.output if i.type == "function_call"]
        self.assertEqual(len(calls), 2 if parallel else 1)
        sent = last_body()
        self.assertEqual(sent["input"], "Weather for <SECRET_1>?")
        self.assertNotIn("ghp_", json.dumps(sent))
        # The application rebuilds the history from the listed keys only (contract: safe replay conversion).
        history: list[dict[str, Any]] = [{"role": "user", "content": f"Weather for {secret(1)}?"}]
        history += [{"type": "function_call", "call_id": k.call_id, "name": k.name, "arguments": k.arguments} for k in calls]
        history += [
            {"type": "function_call_output", "call_id": k.call_id, "output": json.dumps({"temp_c": 21, "note": secret(2), "city": json.loads(k.arguments)["city"]})}
            for k in calls
        ]
        second = c.responses.create(model="qual-resp-json-ok", input=history, store=False, tools=[WEATHER])
        self.assertEqual(second.output_text, STREAM_TEXT)
        body = last_body()
        self.assertIs(body["store"], False)
        self.assertEqual(len(body["input"]), 1 + 2 * len(calls))
        self.assertEqual(body["input"][0], {"role": "user", "content": "Weather for <SECRET_1>?"})
        for i, k in enumerate(calls):
            item = body["input"][1 + i]
            self.assertEqual(sorted(item), ["arguments", "call_id", "name", "type"], "only the listed keys were sent")
            self.assertEqual(item["call_id"], k.call_id)
            self.assertEqual(item["arguments"], k.arguments, "compact arguments are byte-identical after re-encoding")
            result = body["input"][1 + len(calls) + i]
            self.assertEqual(result["type"], "function_call_output")
            self.assertIn("<SECRET_", result["output"])
            self.assertNotIn("ghp_", result["output"])
        seen = provider.calls()
        self.assertEqual((len(seen["calls"]), seen["connections"]), (2, 2))
        OBSERVATIONS.append({"case": "tool round trip, two calls" if parallel else "tool round trip, one call", "provider_requests": 2, "accepted": True})

    def test_one_call_round_trip(self) -> None:
        self.round_trip(False)

    def test_parallel_calls_round_trip(self) -> None:
        self.round_trip(True)

    def test_replaying_response_output_unchanged_is_rejected_and_sends_nothing_further(self) -> None:
        c = client(GATEWAYS["concurrent"])
        first = c.responses.create(model="qual-resp-tool-call", input="weather?", store=False, tools=[WEATHER])
        self.assertEqual(len(provider.calls()["calls"]), 1)
        replay = [{"role": "user", "content": "weather?"}, *[i.model_dump(exclude_none=True) for i in first.output], {"type": "function_call_output", "call_id": "call_qual_1", "output": "sunny"}]
        with self.assertRaises(openai.APIStatusError) as caught:
            c.responses.create(model="qual-resp-json-ok", input=replay, store=False)
        self.assertEqual((caught.exception.status_code, caught.exception.code), (422, "unsupported_input"))
        self.assertEqual(len(provider.calls()["calls"]), 1, "the rejected replay delivered nothing")
        OBSERVATIONS.append({"case": "replay response.output verbatim", "status": 422, "code": "unsupported_input"})

    def test_an_output_for_a_call_that_was_never_made_is_refused_before_any_upstream_contact(self) -> None:
        with self.assertRaises(openai.APIStatusError) as caught:
            client(GATEWAYS["concurrent"]).responses.create(
                model="qual-resp-json-ok", store=False, input=[{"type": "function_call_output", "call_id": "call_never", "output": "x"}]
            )
        self.assertEqual(caught.exception.status_code, 422)
        assert_nothing_upstream(self, "linkage")


class Flat(BaseModel):
    city: str = Field(description="City name")
    days: int


class Documented(BaseModel):
    """Weather lookup for one city."""

    city: str
    days: int


class Inner(BaseModel):
    city: str


class Nested(BaseModel):
    place: Inner
    days: int


class Unit(str, Enum):
    C = "c"
    F = "f"


class WithEnumClass(BaseModel):
    unit: Unit


class WithLiteral(BaseModel):
    unit: Literal["c", "f"]
    city: str


class WithDefault(BaseModel):
    city: str = "Seoul"


class WithPattern(BaseModel):
    code: str = Field(pattern=r"^[A-Z]{3}$")


class WithDatetime(BaseModel):
    at: datetime.datetime


class WithUuid(BaseModel):
    ident: uuid.UUID


class WithBounds(BaseModel):
    city: str = Field(min_length=1, max_length=64)
    days: int = Field(ge=1, le=7)


class WithList(BaseModel):
    cities: list[str] = Field(max_length=5)


# (id, accepted, why, runner)
Run = Callable[[Any], Any]
PROBES: list[tuple[str, bool, str, Run]] = [
    ("responses.parse text_format flat model", True, "title is allowed (ADR 0029); flat Pydantic output", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=Flat)),
    ("responses.parse text_format with Literal", True, "Literal becomes an inline enum", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithLiteral)),
    ("responses.parse text_format with min/max bounds", True, "bounds are in the subset", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithBounds)),
    ("responses.parse text_format with list[str] and max_length", True, "arrays with items are in the subset", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithList)),
    ("responses.parse text_format nested model", False, "$defs and $ref", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=Nested)),
    ("responses.parse text_format Enum class", False, "$defs and $ref", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithEnumClass)),
    ("responses.parse text_format field default", False, "default keyword", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithDefault)),
    ("responses.parse text_format Field(pattern=...)", False, "pattern keyword", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithPattern)),
    ("responses.parse text_format datetime", False, "format keyword", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithDatetime)),
    ("responses.parse text_format UUID", False, "format keyword", lambda c: c.responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=WithUuid)),
    ("responses.parse tools=[pydantic_function_tool(model with docstring)]", True, "description comes from the docstring", lambda c: c.responses.parse(model="qual-resp-tool-call", input="q", store=False, tools=[openai.pydantic_function_tool(Documented)])),
    ("responses.parse tools=[pydantic_function_tool(model, description=...)]", True, "description given explicitly", lambda c: c.responses.parse(model="qual-resp-tool-call", input="q", store=False, tools=[openai.pydantic_function_tool(Flat, description="Weather lookup")])),
    ("responses.parse tools=[pydantic_function_tool(model without docstring)]", False, "the helper writes description: null and null is rejected", lambda c: c.responses.parse(model="qual-resp-tool-call", input="q", store=False, tools=[openai.pydantic_function_tool(Flat)])),
    ("responses.parse tools=[pydantic_function_tool(nested model)]", False, "$defs and $ref", lambda c: c.responses.parse(model="qual-resp-tool-call", input="q", store=False, tools=[openai.pydantic_function_tool(Nested, description="d")])),
    ("pydantic_function_tool(Model) output passed to responses.create (no SDK conversion)", False, "Chat-shaped tool: create does not convert it", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[openai.pydantic_function_tool(Documented)])),
    ("hand-built flat function tool", True, "the documented flat shape", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[WEATHER])),
    ("hand-built flat tool with parameters None and strict None", True, "both required keys present", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[{"type": "function", "name": "ping", "description": "d", "parameters": None, "strict": None}])),
    ("Chat-shaped tool {type, function:{...}}", False, "flat tool has no function wrapper", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[{"type": "function", "function": {"name": "t", "parameters": {"type": "object"}}}])),
    ("Chat-shaped response_format via extra_body", False, "unknown top-level field", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, extra_body={"response_format": {"type": "json_object"}})),
    ("SDK default call without store", False, "store is required to be false and the SDK does not send it", lambda c: c.responses.create(model="qual-resp-json-ok", input="q")),
    ("tool_choice in the Chat shape", False, "flat form only", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[WEATHER], tool_choice={"type": "function", "function": {"name": "get_weather"}})),
    ("tool_choice flat function form", True, "documented form", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[WEATHER], tool_choice={"type": "function", "name": "get_weather"})),
    ("hosted web_search tool", False, "hosted tools are rejected", lambda c: c.responses.create(model="qual-resp-json-ok", input="q", store=False, tools=[{"type": "web_search"}])),
]


class HelperOutput(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()


def add_probe(index: int, probe_id: str, accepted: bool, why: str, run: Run) -> None:
    def test(self: HelperOutput) -> None:
        status, code, rendered = 200, None, ""
        try:
            run(client(GATEWAYS["concurrent"]))
        except openai.APIStatusError as err:
            status, code = err.status_code, err.code
            rendered = json.dumps({"m": err.message, "b": err.body}, default=str)
        except ValidationError:
            pass  # the request was accepted and answered; the fixed fake reply just does not fit this model
        seen = provider.calls()
        self.assertEqual(leaks(rendered), [])
        if accepted:
            self.assertEqual(status, 200, f"{probe_id}: {code}")
            self.assertEqual(len(seen["calls"]), 1)
            self.assertEqual(seen["calls"][0]["path"], "/v1/responses")
        else:
            self.assertEqual(status, 422, f"{probe_id}: expected a 422, got {status}")
            self.assertEqual(code, "unsupported_input")
            assert_nothing_upstream(self, probe_id)
        OBSERVATIONS.append({"helper": probe_id, "accepted": accepted, "status": status, "code": code, "provider_requests": len(seen["calls"])})

    test.__name__ = f"test_{index:02d}_{'accepted' if accepted else 'rejected'}"
    test.__doc__ = f"{'accepted' if accepted else 'rejected'}: {probe_id} ({why})"
    setattr(HelperOutput, test.__name__, test)


for _i, (_id, _ok, _why, _run) in enumerate(PROBES):
    add_probe(_i, _id, _ok, _why, _run)


class ParsedResults(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_text_format_description_is_redacted_and_the_parse_helper_returns_the_typed_value(self) -> None:
        class Described(BaseModel):
            city: str = Field(description=f"The city {secret(7)}")
            days: int

        reply = client(GATEWAYS["concurrent"]).responses.parse(model="qual-resp-json-structured", input="q", store=False, text_format=Described)
        self.assertEqual(reply.output_parsed.model_dump(), {"city": "Seoul", "days": 3})
        sent = last_body()
        self.assertEqual(sent["text"]["format"]["schema"]["properties"]["city"]["description"], "The city <SECRET_1>")
        self.assertNotIn("ghp_", json.dumps(sent))

    def test_pydantic_function_tool_parse_returns_typed_arguments_from_the_relayed_function_call(self) -> None:
        reply = client(GATEWAYS["concurrent"]).responses.parse(
            model="qual-resp-tool-call", input="q", store=False, tools=[openai.pydantic_function_tool(Documented, name="get_weather")]
        )
        call = next(i for i in reply.output if i.type == "function_call")
        self.assertEqual(call.parsed_arguments.model_dump(), {"city": "Seoul", "days": 3})


if __name__ == "__main__":
    unittest.main()
