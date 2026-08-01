"""Unit tests for provider HTTP error classification
(bigtiny/providers/errors.py). Run: pytest tests/test_provider_errors.py -v
"""

from __future__ import annotations

import json

from bigtiny.providers.errors import classify_provider_error


def test_classifies_openai_context_length_exceeded():
    body = json.dumps({
        "error": {
            "message": "This model's maximum context length is 8192 tokens.",
            "type": "invalid_request_error",
            "code": "context_length_exceeded",
        }
    })
    result = classify_provider_error(400, body)
    assert result.type == "context_exceeded"
    assert "context limit" in result.user_message.lower() or "context" in result.user_message.lower()
    assert "{" not in result.user_message  # no raw JSON leaking into user-facing text


def test_classifies_anthropic_insufficient_quota():
    body = json.dumps({
        "error": {"type": "insufficient_quota", "message": "You have insufficient quota."}
    })
    result = classify_provider_error(400, body)
    assert result.type == "insufficient_credits"
    assert "{" not in result.user_message


def test_classifies_generic_context_exceeded_from_message_text():
    body = json.dumps({"error": {"message": "prompt is too long: maximum context length exceeded"}})
    result = classify_provider_error(400, body)
    assert result.type == "context_exceeded"


def test_classifies_402_as_insufficient_credits():
    body = json.dumps({"error": {"message": "Payment required"}})
    result = classify_provider_error(402, body)
    assert result.type == "insufficient_credits"


def test_returns_other_for_unrecognised_error():
    body = json.dumps({"error": {"message": "internal server hiccup"}})
    result = classify_provider_error(500, body)
    assert result.type == "other"
    assert result.raw_message == "internal server hiccup"


def test_handles_non_json_error_body_gracefully():
    result = classify_provider_error(500, "<html>502 Bad Gateway</html>")
    assert result.type == "other"
    assert result.raw_message == "<html>502 Bad Gateway</html>"


def test_handles_empty_error_body():
    result = classify_provider_error(401, "")
    assert result.type == "other"
    assert result.user_message  # non-empty fallback


def test_user_messages_are_never_empty_and_carry_no_raw_json():
    for status, body in [
        (400, json.dumps({"error": {"code": "context_length_exceeded", "message": "too long"}})),
        (402, json.dumps({"error": {"message": "billing issue"}})),
        (500, json.dumps({"error": {"message": "weird"}})),
        (500, "not json at all"),
    ]:
        result = classify_provider_error(status, body)
        assert result.user_message
        assert "{" not in result.user_message
