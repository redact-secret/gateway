"""Alpha 2 behavior that needs the SDK itself (#57), pinned OpenAI Python SDK.

The SDK's REAL schema helpers (`openai.pydantic_function_tool`, the `parse` helper's
`response_format=PydanticModel` conversion), a tool round trip driven by typed SDK objects, the
verbatim-replay incompatibility, and request-state isolation under concurrency. The row-by-row
field and policy matrix is test_alpha2_cases.py.
"""

import copy
import json
import sys
import unittest
import uuid
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from typing import Any, Literal, Optional
from uuid import UUID

import openai
from alpha2_support import normalize, planted_in
from pydantic import BaseModel, Field, create_model
from qual_support import GATEWAYS, SYN, STREAM_TEXT, assert_nothing_upstream, client, leaks, provider, secret, write_evidence

OBSERVATIONS: dict[str, Any] = {}
USER = {"role": "user", "content": "weather in Seoul and 서울?"}


def weather_model(n: int) -> type[BaseModel]:
    """A flat model whose field description carries a synthetic secret."""
    return create_model(
        "Weather",
        __doc__="Weather arguments.",
        city=(str, Field(description=f"City name {secret(n)}")),
        days=(int, Field(ge=1, le=7)),
        unit=(Literal["c", "f"], ...),
        note=(Optional[str], ...),
        tags=(list[str], ...),
    )


class GetWeather(BaseModel):
    city: str
    days: int


class Address(BaseModel):
    city: str


def rejected(test: unittest.TestCase, why: str, **params: Any) -> openai.APIStatusError:
    provider.reset()
    with test.assertRaises(openai.APIStatusError, msg=why) as caught:
        client(GATEWAYS["standard"]).chat.completions.create(**params)
    err = caught.exception
    test.assertEqual((err.status_code, err.code), (422, "unsupported_input"), why)
    test.assertEqual(leaks(json.dumps({"m": err.message, "b": err.body}, default=str)), [], why)
    assert_nothing_upstream(test, why)
    return err


class PydanticHelpers(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_pydantic_function_tool_flat_model_is_forwarded_as_emitted_with_the_secret_redacted(self) -> None:
        tool = openai.pydantic_function_tool(weather_model(1), name="get_weather", description="Look up weather.")
        # `title` is what the strict conversion adds to every schema object (ADR 0029 admits it).
        self.assertIn("title", tool["function"]["parameters"])
        res = client(GATEWAYS["standard"]).chat.completions.create(
            model="qual-json-ok", messages=[USER], tools=[tool], tool_choice="auto", parallel_tool_calls=False
        )
        self.assertEqual(res.choices[0].message.content, STREAM_TEXT)
        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), 1)
        sent = json.loads(calls[0]["body"])
        expected_tool = json.loads(json.dumps(tool).replace(secret(1), "<SECRET_1>"))
        self.assertEqual(sent["tools"], [expected_tool], "the SDK helper's tool, only the description redacted")
        self.assertEqual(planted_in(calls[0]["body"]), [])
        fn = sent["tools"][0]["function"]
        self.assertIs(fn["strict"], True)
        self.assertIs(fn["parameters"]["additionalProperties"], False)
        self.assertEqual(fn["parameters"]["required"], ["city", "days", "unit", "note", "tags"])
        OBSERVATIONS["pydantic_function_tool_flat"] = "forwarded as emitted (title kept, description redacted)"

    def test_parse_helper_response_format_round_trip(self) -> None:
        class Answer(BaseModel):
            city: str = Field(description=f"City {secret(2)}")
            days: int

        res = client(GATEWAYS["standard"]).chat.completions.parse(
            model="qual-json-structured", messages=[USER], response_format=Answer
        )
        parsed = res.choices[0].message.parsed
        self.assertEqual(parsed, Answer(city="Seoul", days=3), "the SDK parsed the relayed structured reply")
        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), 1)
        sent = json.loads(calls[0]["body"])["response_format"]
        self.assertEqual(sent["type"], "json_schema")
        self.assertEqual(sent["json_schema"]["name"], "Answer")
        self.assertIs(sent["json_schema"]["strict"], True)
        self.assertEqual(sent["json_schema"]["schema"]["properties"]["city"]["description"], "City <SECRET_1>")
        self.assertEqual(sent["json_schema"]["schema"]["properties"]["city"]["title"], "City")
        self.assertEqual(planted_in(calls[0]["body"]), [])
        OBSERVATIONS["parse_response_format_flat"] = "forwarded; parsed instance returned"

    def test_raw_model_json_schema_of_a_flat_model_is_accepted_but_not_the_features_below(self) -> None:
        schema = Address.model_json_schema()  # title kept, no default, no $defs
        client(GATEWAYS["standard"]).chat.completions.create(
            model="qual-json-ok", messages=[USER], tools=[{"type": "function", "function": {"name": "f", "parameters": schema}}]
        )
        self.assertEqual(json.loads(provider.calls()["calls"][0]["body"])["tools"][0]["function"]["parameters"], schema)

    def test_pydantic_features_that_emit_unsupported_keywords_are_rejected_with_zero_upstream(self) -> None:
        class Nested(BaseModel):
            addr: Address

        class WithDefault(BaseModel):
            days: int = 3

        class WithPattern(BaseModel):
            code: str = Field(pattern="^[A-Z]{3}$")

        class WithDatetime(BaseModel):
            when: datetime

        class WithUuid(BaseModel):
            ident: UUID

        found: dict[str, list[str]] = {}
        for name, model, keywords in [
            ("nested model ($defs/$ref)", Nested, ["$defs", "$ref"]),
            ("field default", WithDefault, ["default"]),
            ("Field(pattern=...)", WithPattern, ["pattern"]),
            ("datetime (format)", WithDatetime, ["format"]),
            ("UUID (format)", WithUuid, ["format"]),
        ]:
            tool = openai.pydantic_function_tool(model, name="t")
            text = json.dumps(tool)
            for keyword in keywords:
                self.assertIn(f'"{keyword}"', text, f"{name}: the helper emits {keyword}")
            rejected(self, f"{name} as a tool", model="qual-json-ok", messages=[USER], tools=[tool])
            rejected(
                self,
                f"{name} as response_format",
                model="qual-json-ok",
                messages=[USER],
                response_format=openai.lib._parsing.type_to_response_format_param(model),
            )
            found[name] = keywords
        OBSERVATIONS["pydantic_rejected_features"] = found


class ToolRoundTrips(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def round_trip(self, model: str, expected_calls: int) -> None:
        c = client(GATEWAYS["standard"])
        tool = openai.pydantic_function_tool(GetWeather, name="get_weather")
        first = c.chat.completions.parse(
            model=model, messages=[USER], tools=[tool], tool_choice="auto", parallel_tool_calls=expected_calls > 1
        )
        message = first.choices[0].message
        self.assertEqual(first.choices[0].finish_reason, "tool_calls")
        self.assertEqual(len(message.tool_calls), expected_calls)
        self.assertEqual(message.tool_calls[0].function.parsed_arguments, GetWeather(city="Seoul", days=3))
        # The application replays only the documented fields: role, content (null), id/type/function.
        assistant = {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {"id": tc.id, "type": "function", "function": {"name": tc.function.name, "arguments": tc.function.arguments}}
                for tc in message.tool_calls
            ],
        }
        results = [
            {"role": "tool", "tool_call_id": tc["id"], "content": f"result {i}: sunny, key {secret(10 + i)}"}
            for i, tc in enumerate(assistant["tool_calls"], start=1)
        ]
        second = c.chat.completions.create(model="qual-json-ok", messages=[USER, assistant, *results], tools=[tool])
        self.assertEqual(second.choices[0].message.content, STREAM_TEXT)

        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), 2, "one request per SDK call")
        sent = json.loads(calls[1]["body"])
        expected = {
            "model": "qual-json-ok",
            "messages": [
                USER,
                assistant,
                *[{**r, "content": f"result {i}: sunny, key <SECRET_{i}>"} for i, r in enumerate(results, start=1)],
            ],
            "tools": [json.loads(json.dumps(tool))],
        }
        self.assertEqual(normalize(sent)[0], normalize(expected)[0])
        self.assertEqual(planted_in(calls[1]["body"]), [])
        OBSERVATIONS[f"round_trip_{model}"] = {"upstream_requests": 2, "tool_results": expected_calls, "placeholders": expected_calls}

    def test_one_call(self) -> None:
        self.round_trip("qual-json-tool-call", 1)

    def test_parallel_calls(self) -> None:
        self.round_trip("qual-json-tool-calls-parallel", 2)

    def test_replaying_the_providers_message_object_verbatim_is_rejected(self) -> None:
        c = client(GATEWAYS["standard"])
        tool = openai.pydantic_function_tool(GetWeather, name="get_weather")
        first = c.chat.completions.parse(model="qual-json-tool-call", messages=[USER], tools=[tool])
        message = first.choices[0].message
        tool_message = {"role": "tool", "tool_call_id": message.tool_calls[0].id, "content": "ok"}
        self.assertEqual(len(provider.calls()["calls"]), 1)
        for how, assistant in [
            ("the SDK message object", message),
            ("message.model_dump(exclude_none=True)", message.model_dump(exclude_none=True)),
        ]:
            with self.assertRaises(openai.APIStatusError, msg=how) as caught:
                c.chat.completions.create(model="qual-json-ok", messages=[USER, assistant, tool_message])
            self.assertEqual((caught.exception.status_code, caught.exception.code), (422, "unsupported_input"), how)
            self.assertEqual(len(provider.calls()["calls"]), 1, f"{how}: nothing more reached the provider")
        OBSERVATIONS["verbatim_message_replay"] = "rejected 422 unsupported_input (refusal / annotations / parsed fields)"


class Isolation(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_placeholder_numbering_and_bodies_are_per_request_when_24_requests_run_at_once(self) -> None:
        total = 24

        def plan(i: int) -> tuple[str, int, dict[str, Any], dict[str, Any]]:
            marker = f"{SYN['prompt_marker_prefix']}{uuid.uuid4()}"
            k = i % 4 + 1

            def make(ph: Any) -> dict[str, Any]:
                messages: list[dict[str, Any]] = [{"role": "user", "content": f"req {i} {marker} {ph(1)}"}]
                if k >= 2:
                    messages.append(
                        {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {"id": f"call_{i}", "type": "function", "function": {"name": "lookup", "arguments": json.dumps({"q": ph(2), "n": i})}}
                            ],
                        }
                    )
                    messages.append({"role": "tool", "tool_call_id": f"call_{i}", "content": f"found {ph(3)}" if k >= 3 else "found"})
                body: dict[str, Any] = {"model": "qual-json-ok", "messages": messages}
                if k >= 4:
                    body["metadata"] = {"trace": f"t-{i} {ph(4)}"}
                return body

            return marker, k, make(lambda j: secret(1000 * (i + 1) + j)), make(lambda j: f"<SECRET_{j}>")

        plans = [plan(i) for i in range(total)]
        pem = "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----"
        bad = [
            {"model": "qual-json-ok", "messages": [USER], "tools": [{"type": "function", "function": {"name": secret(7)}}]},
            {"model": "qual-json-ok", "messages": [{"role": "user", "content": pem}]},
        ]

        def send(params: dict[str, Any], retries: int) -> Any:
            try:
                return client(GATEWAYS["concurrent"], max_retries=retries).chat.completions.create(**params)
            except openai.APIStatusError as err:
                return err

        with ThreadPoolExecutor(max_workers=total + len(bad)) as pool:
            futures = [pool.submit(send, p[2], 4) for p in plans] + [pool.submit(send, b, 0) for b in bad]
            results = [f.result() for f in futures]
        for i, res in enumerate(results[:total]):
            self.assertNotIsInstance(res, Exception, f"request {i} completed")
        for res in results[total:]:
            self.assertIsInstance(res, openai.APIStatusError)
            self.assertEqual(res.status_code, 422)
        calls = provider.calls()["calls"]
        self.assertEqual(len(calls), total, "one upstream request per forwarded request, none for the rejected neighbours")
        for i, (marker, k, _params, expected) in enumerate(plans):
            mine = [c for c in calls if marker in c["body"]]
            self.assertEqual(len(mine), 1, f"request {i}: exactly one upstream request")
            self.assertEqual(
                normalize(json.loads(mine[0]["body"]))[0],
                normalize(copy.deepcopy(expected))[0],
                f"request {i}: numbering restarts at 1 and no other request's text appears",
            )
            self.assertEqual(mine[0]["body"].count("<SECRET_"), k, f"request {i}: placeholder count")
        self.assertEqual(planted_in("".join(c["body"] for c in calls)), [])
        OBSERVATIONS["concurrency"] = {"concurrent_forwarded": total, "rejected_neighbours": len(bad), "upstream_requests": len(calls)}


def tearDownModule() -> None:
    write_evidence(
        "alpha2-sdk-python.json",
        {"sdk": f"openai (PyPI) {openai.__version__}", "python": sys.version.split()[0], "observations": OBSERVATIONS},
    )


if __name__ == "__main__":
    unittest.main()
