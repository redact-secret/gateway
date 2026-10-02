"""JSON responses, provider errors, and gateway transport errors through the pinned Python SDK."""

import unittest

import openai
from qual_support import GATEWAYS, STREAM_TEXT, SYN, client, provider

MSG = [{"role": "user", "content": "hello"}]


def failure(gateway: str, model: str) -> openai.APIStatusError:
    try:
        client(GATEWAYS[gateway]).chat.completions.create(model=model, messages=MSG)
    except openai.APIStatusError as err:
        return err
    raise AssertionError("expected the request to fail")


class JsonAndErrors(unittest.TestCase):
    def setUp(self) -> None:
        provider.reset()

    def test_large_completion_is_relayed_intact(self) -> None:
        res = client(GATEWAYS["standard"]).chat.completions.create(model="qual-json-large", messages=MSG)
        self.assertEqual(len(res.choices[0].message.content), len("synthetic large reply ") * 3000)

    def test_provider_statuses_and_bodies_are_relayed_unchanged(self) -> None:
        expected = {
            400: openai.BadRequestError,
            401: openai.AuthenticationError,
            403: openai.PermissionDeniedError,
            404: openai.NotFoundError,
            409: openai.ConflictError,
            422: openai.UnprocessableEntityError,
            429: openai.RateLimitError,
            500: openai.InternalServerError,
            503: openai.InternalServerError,
        }
        for status, cls in expected.items():
            with self.subTest(status=status):
                err = failure("standard", f"qual-err-{status}")
                self.assertEqual(err.status_code, status)
                self.assertIsInstance(err, cls)
                self.assertEqual(
                    err.body,
                    {"message": f"synthetic provider error {status}", "type": "synthetic_error", "param": None, "code": f"synthetic_{status}"},
                    "provider JSON error body arrives unchanged (not the gateway envelope)",
                )

    def test_oversize_provider_response_is_a_safe_gateway_502(self) -> None:
        err = failure("standard", "qual-json-oversize")
        self.assertEqual((err.status_code, err.code), (502, "upstream_response_too_large"))

    def test_truncated_provider_response_is_502_invalid_response(self) -> None:
        err = failure("standard", "qual-json-truncated")
        self.assertEqual((err.status_code, err.code), (502, "upstream_invalid_response"))

    def test_unreachable_provider_is_502_unavailable(self) -> None:
        err = failure("dead_provider", "qual-json-ok")
        self.assertEqual((err.status_code, err.code), (502, "upstream_unavailable"))

    def test_provider_that_never_replies_is_504_timeout(self) -> None:
        err = failure("tight", "qual-hang")
        self.assertEqual((err.status_code, err.code), (504, "upstream_timeout"))

    def test_gateway_errors_carry_the_fixed_envelope_only(self) -> None:
        err = failure("no_upstream", "qual-json-ok")
        self.assertEqual(err.body, {"code": "not_implemented"})
        self.assertNotIn(SYN["api_key"], str(err.body))
        res = client(GATEWAYS["standard"]).chat.completions.create(model="qual-json-ok", messages=MSG)
        self.assertEqual(res.choices[0].message.content, STREAM_TEXT)


if __name__ == "__main__":
    unittest.main()
