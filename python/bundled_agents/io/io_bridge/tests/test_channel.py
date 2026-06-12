"""io_bridge — unit tests for the channel-model contract (the descriptor + the message
credential extractor + the teaching-pointer decision fields)."""

from __future__ import annotations

from io_bridge import (
    ALLOW,
    Channel,
    CredentialExtractor,
    Decision,
    EnvelopeExtractor,
)
from io_bridge.ingress_rules import DenyInbound


def test_envelope_extractor_pulls_token_off_the_frame():
    ex = EnvelopeExtractor()
    assert isinstance(ex, CredentialExtractor)
    assert ex.extract({"type": "call", "auth_token": "tok"}) == "tok"
    # the token rides the ENVELOPE — a token buried in the payload is NOT extracted
    assert ex.extract({"type": "call", "payload": {"auth_token": "tok"}}) is None
    assert ex.extract({"type": "call"}) is None  # absent
    assert ex.extract({"auth_token": 123}) is None  # non-str ignored
    assert ex.extract("not-a-frame") is None  # wrong carrier type


def test_channel_descriptor_binds_the_five_facts():
    ch = Channel(
        direction="ingress",
        modality="message",
        transport="ws",
        rule=DenyInbound(),
        extractor=EnvelopeExtractor(),
    )
    assert ch.direction == "ingress"
    assert ch.modality == "message"
    assert ch.transport == "ws"
    assert isinstance(ch.rule, DenyInbound)
    assert isinstance(ch.extractor, EnvelopeExtractor)


def test_decision_teaching_pointer_defaults_empty():
    # the hint/see fields are the declared teaching-denial contract; unused until #9
    assert ALLOW.allowed and ALLOW.hint == "" and ALLOW.see == ""
    d = Decision(False, "sealed", hint="open it via ingress_rule", see="")
    assert d.hint and d.see == ""
