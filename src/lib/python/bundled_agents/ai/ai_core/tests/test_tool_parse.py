"""tool_parse — the non-streaming parser. The streaming parser and the agentic
loop are gone (dispatch is the emergent mind's job), but this module has TWO live
consumers: `core._route_reply`, which scans each completion and dispatches what it
names, and `core._derive_reaction`, the context-reaction read-model. This docstring
claimed the read-model was the only one until 2026-08-06."""

from __future__ import annotations

import asyncio
import pytest
from ai_core.tool_parse import extract_tool_calls, malformed_calls, parse_one

# ─── parse_one (lenient JSON → call) ────────────────────────────


def test_parse_one_canonical():
    c = parse_one(
        '{"name": "send", "arguments": {"target_id": "core", "payload": {"type": "list_agents"}}}'
    )
    assert c["name"] == "send"
    assert c["arguments"] == {"target_id": "core", "payload": {"type": "list_agents"}}
    assert c["id"].startswith("call_")


def test_parse_one_flattened_and_tool_alias():
    # tiny models drift: `tool` alias for name, flattened args
    c = parse_one(
        '{"tool": "send", "target_id": "foo", "payload": {"type": "reflect"}}'
    )
    assert c["name"] == "send"
    assert c["arguments"] == {"target_id": "foo", "payload": {"type": "reflect"}}


def test_parse_one_stringified_arguments():
    c = parse_one('{"name": "send", "arguments": "{\\"target_id\\": \\"x\\"}"}')
    assert c["arguments"] == {"target_id": "x"}


def test_parse_one_malformed_returns_none():
    assert parse_one("{not json") is None
    assert parse_one("[1,2,3]") is None


# ─── extract (non-streaming, for the history reader) ────────────


def test_extract_tool_calls():
    text = (
        "doing it "
        '<tool_call>{"name":"send","arguments":{"target_id":"self","payload":{"type":"recall"}}}</tool_call>'
        " and "
        '<tool_call>{"name":"send","arguments":{"target_id":"mem","payload":{"type":"set"}}}</tool_call>'
    )
    calls = extract_tool_calls(text)
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["recall", "set"]


def test_extract_ignores_unterminated_and_malformed():
    assert extract_tool_calls("<tool_call>{unterminated") == []
    assert extract_tool_calls("<tool_call>{bad}</tool_call>") == []


def test_extract_channel_envelope_verbatim_from_the_failed_run():
    """VERBATIM from the 2026-08-04 run that died at pass 1: gemma-4-12B-it planned
    correctly, picked the right agent and verb, and wrapped it in its OWN template's
    shape. `<|tool_call>` is not `<tool_call>` — the pipe breaks the match — so zero
    calls were extracted and the chain ended with the model never getting a reply."""
    calls = extract_tool_calls(
        "Let's start by listing the agents."
        "<|tool_call>call:kernel_state.list_agents{}<tool_call|>"
    )
    assert len(calls) == 1
    assert calls[0]["name"] == "send"
    assert calls[0]["arguments"] == {
        "target_id": "kernel_state",
        "payload": {"type": "list_agents"},
    }


def test_channel_strings_use_their_own_token_and_may_contain_commas():
    """The documented shape (ai.google.dev). Strings wear `<|"|>`, NOT `"` — and a
    comma inside one must not split the argument list, which is exactly what a
    `split(",")` or a `json.loads` would get wrong."""
    calls = extract_tool_calls(
        '<|tool_call>call:weather.get{location:<|"|>Tokyo, JP<|"|>}<tool_call|>'
    )
    assert calls[0]["arguments"] == {
        "target_id": "weather",
        "payload": {"type": "get", "location": "Tokyo, JP"},
    }


def test_channel_args_land_in_the_payload_beside_type():
    """`call:<target>.<verb>{…}` IS the universal envelope written differently: the
    braces are payload fields alongside `type`, not a second argument slot."""
    calls = extract_tool_calls(
        '<|tool_call>call:assets.write{path:<|"|>a.txt<|"|>,content:<|"|>x<|"|>}<tool_call|>'
    )
    assert calls[0]["arguments"] == {
        "target_id": "assets",
        "payload": {"type": "write", "path": "a.txt", "content": "x"},
    }


def test_channel_bare_scalars_and_nested_objects():
    """Numbers, booleans and nested objects are written BARE in this grammar. Also
    covers the uncompressed shape — the model naming our taught `send` tool directly."""
    calls = extract_tool_calls(
        '<|tool_call>call:send{target_id:<|"|>m<|"|>,'
        'payload:{type:<|"|>write<|"|>,count:3,ratio:0.5,ok:true,note:null}}<tool_call|>'
    )
    assert calls[0]["name"] == "send"
    assert calls[0]["arguments"] == {
        "target_id": "m",
        "payload": {
            "type": "write",
            "count": 3,
            "ratio": 0.5,
            "ok": True,
            "note": None,
        },
    }


def test_channel_envelope_with_json_quoted_args():
    """VERBATIM from the 88-pass run of 2026-08-04. The envelope is the channel one,
    the arguments are JSON — models mix the two. Reading the value as a bare token
    kept its quote characters, so `id` never arrived and all THREE posture-changing
    calls came back `{"error": "update_agent: id required"}`."""
    calls = extract_tool_calls(
        "<|tool_call>call:kernel_state.update_agent"
        '{"id":"assets","ingress_rule":"allow_all_ingress"}<tool_call|>'
    )
    assert calls[0]["arguments"] == {
        "target_id": "kernel_state",
        "payload": {
            "type": "update_agent",
            "id": "assets",
            "ingress_rule": "allow_all_ingress",
        },
    }


def test_channel_args_carrying_target_id_are_a_plain_send():
    """VERBATIM from the same run. `target_id` in the args means the model wrote
    canonical `send` arguments and the dotted prefix is a namespace it invented, not
    a target. Reading the prefix as the target produced
    `{"error": "kernel_state: unknown type 'send'"}` eight times."""
    calls = extract_tool_calls(
        "<|tool_call>call:kernel_state.send"
        '{"payload":{"type":"reflect"},"target_id":"assets"}<tool_call|>'
    )
    assert calls[0]["name"] == "send"
    assert calls[0]["arguments"] == {
        "target_id": "assets",
        "payload": {"type": "reflect"},
    }


def test_channel_target_id_without_payload_keeps_the_verb():
    """VERBATIM from the passing run of 2026-08-04. `call:X.reflect{"target_id":"Y"}`
    means "reflect Y". An earlier revision saw `target_id` and emitted a send with NO
    payload; `_route_reply` dispatched `None` and the handler raised
    `'NoneType' object has no attribute 'get'` three times in one run."""
    calls = extract_tool_calls(
        '<|tool_call>call:kernel_state.reflect{"target_id":"transcripts"}<tool_call|>'
    )
    assert calls[0]["arguments"] == {
        "target_id": "transcripts",
        "payload": {"type": "reflect"},
    }


def test_channel_send_with_neither_payload_nor_verb_is_not_a_call():
    assert (
        extract_tool_calls('<|tool_call>call:send{"target_id":"x"}<tool_call|>') == []
    )


def test_channel_malformed_is_dropped_not_repaired():
    """A parser that guesses what the model meant is measuring its own guess."""
    assert extract_tool_calls("<|tool_call>call:a.b{unterminated<tool_call|>") == []
    assert extract_tool_calls('<|tool_call>call:a.b{k:<|"|>no close}<tool_call|>') == []
    assert extract_tool_calls("<|tool_call>garbage<tool_call|>") == []


def test_three_envelopes_interleave_in_document_order():
    """All three shapes coexist; `<|tool_call>` must not shadow or be shadowed by
    the taught `<tool_call>` envelope."""
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>'
        "<|tool_call>call:n.two{}<tool_call|>"
        '<invoke name="send"><parameter name="arguments">'
        '{"target_id":"o","payload":{"type":"three"}}</parameter></invoke>'
    )
    calls = extract_tool_calls(text)
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["one", "two", "three"]
    assert [c["arguments"]["target_id"] for c in calls] == ["m", "n", "o"]


# ─── malformed_calls (a broken envelope must have a VOICE) ──────

# From gemma-4-12B, probe 05, 2026-08-06 — the call that ended the run. Kept byte for
# byte EXCEPT the handler_module, retargeted to the current executor when the
# per-language runtimes were deleted; the defect under test is the unclosed `code`
# string, which is untouched. The program body is escaped correctly; the `code` value
# is never closed, so every brace after it is string content and the object runs off
# the end.
GEMMA_UNCLOSED_CODE = (
    '{"name":"send","arguments":{"target_id":"kernel_state","payload":'
    '{"type":"create_agent","handler_module":"exec_bridge.tools",'
    '"id":"reading_aggregator","code":"def update_totals():\\n'
    '    worker_res = send(\\"reading_worker\\", {\\"type\\": \\"compute_total\\"})\\n'
    '    return f\\"updated to {worker_res}\\"}}'
)


def test_the_unclosed_code_string_that_ended_the_gemma_run_is_reported():
    """The regression this exists for. Silence here read as "the model is done"
    while the task sat untouched, so the assertion is not that we PARSE it —
    we must not — but that the model is TOLD."""
    text = f"<tool_call>{GEMMA_UNCLOSED_CODE}"
    assert extract_tool_calls(text) == [], "a broken call must never dispatch"
    bad = malformed_calls(text)
    assert len(bad) == 1, bad
    assert "NOT dispatched" in bad[0]["error"]
    assert "Unterminated string" in bad[0]["error"], "name the JSON fault"
    assert "send_binary" in bad[0]["error"], "name the RECOVERY (go raw), not more escaping"
    assert "reading_aggregator" in bad[0]["your_block"], "show its own text back"


def test_prose_with_no_envelope_is_silence_not_an_error():
    """The distinction the whole change rests on: emitting no call is a legitimate
    way to stop. Only an envelope that OPENED and failed is a fault."""
    assert malformed_calls("I have finished wiring the aggregator.") == []
    assert malformed_calls("") == []


def test_a_good_call_beside_a_broken_one_reports_only_the_broken_one():
    """A partially-read turn dispatched half the model's intent. Which half was
    lost is not inferable from the replies it got, so the loss is named."""
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"m",'
        '"payload":{"type":"one"}}}</tool_call>'
        '<tool_call>{"name":"send","arguments":{"target_id":"n","code":"oops}}'
    )
    assert [c["arguments"]["target_id"] for c in extract_tool_calls(text)] == ["m"]
    assert len(malformed_calls(text)) == 1


def test_every_envelope_gets_the_same_voice():
    """`<|tool_call>` was the envelope whose silent drop was documented and
    unfixed; it must report like the others now."""
    for text in (
        "<|tool_call>call:a.b{unterminated<tool_call|>",
        '<invoke name="send"><parameter name="arguments">{oops</parameter></invoke>',
    ):
        assert extract_tool_calls(text) == []
        bad = malformed_calls(text)
        assert len(bad) == 1 and "NOT dispatched" in bad[0]["error"], text


def test_prose_mentioning_the_tag_is_not_reported_as_a_broken_call():
    """Measured 2026-08-06: gemma wrote `<tool_call>` inside a sentence and the
    first version of this feature answered with a lecture about JSON, costing a
    turn. An unterminated opener with no call-shaped body is a MENTION."""
    text = "You send it in a <tool_call> block. The `kernel_state` agent is root."
    assert malformed_calls(text) == []


def test_an_unterminated_but_call_shaped_block_still_reports():
    """The other side of that rule — nemotron-3-ultra-550b omits the closing tag
    on real calls, so a `{`-shaped body is an attempt however it ends."""
    assert len(malformed_calls('<tool_call>{"name":"send","arguments":{oops')) == 1


def test_prose_is_not_an_attempt_even_when_a_later_closer_terminates_it():
    """A stray opener in a sentence, plus the closing tag of a REAL call further
    down, forms a "terminated" block out of prose. Requiring the closer let that
    through and CLOSED A LOOP: the error echoes the block, the model quotes the
    error, the quotation contains the tag, the next scan finds a fresh opener.
    Three of run 4's errors on 2026-08-06 were this feeding itself."""
    text = (
        "My previous call was: `<tool_call>` and it failed. Retrying now."
        '<tool_call>{"name":"send","arguments":{"target_id":"m",'
        '"payload":{"type":"one"}}}</tool_call>'
    )
    assert [c["arguments"]["target_id"] for c in extract_tool_calls(text)] == ["m"]
    assert malformed_calls(text) == [], "the prose mention must not be reported"


def test_the_echoed_block_cannot_re_arm_the_scanner():
    """Whatever we echo becomes text the model quotes back. A live `<tool_call>` in
    it is indistinguishable from an emitted one, so the delimiters are defanged."""
    bad = malformed_calls('<tool_call>{"name":"send","note":"see <tool_call> oops')[0]
    echoed = bad["your_block"]
    assert "oops" in echoed, "the text must stay legible"
    assert "<tool_call>" not in echoed, "the tag must not survive verbatim"
    assert malformed_calls(echoed) == [], "echoing it must not produce a new report"


# ─── channel strings: both quote styles ─────────────────────────

# Verbatim from gemma-4-12B, probe 05 re-run, 2026-08-06 — a completely correct
# intent that the parser threw away because the strings were single-quoted. It was
# the model's last act; it had nothing left to fix and the run stalled.
GEMMA_SINGLE_QUOTED = (
    "<|tool_call>call:kernel_state{type: 'send', target_id: 'kernel', "
    "payload: {type: 'reflect', readme: 'index'}, reply_to: 'self'}<tool_call|>"
)


def test_single_quoted_channel_args_parse():
    calls = extract_tool_calls(GEMMA_SINGLE_QUOTED)
    assert len(calls) == 1, "a single-quoted body is a shape models really emit"
    args = calls[0]["arguments"]
    assert args["target_id"] == "kernel"
    assert args["payload"] == {"type": "reflect", "readme": "index"}
    assert args["reply_to"] == "self"
    assert malformed_calls(GEMMA_SINGLE_QUOTED) == []


def test_both_quote_styles_yield_the_same_call():
    """Which quote a model reaches for is a fact about its training, not about our
    grammar, so it must not change the result."""
    double = GEMMA_SINGLE_QUOTED.replace("'", '"')
    assert (
        extract_tool_calls(double)[0]["arguments"]
        == (extract_tool_calls(GEMMA_SINGLE_QUOTED)[0]["arguments"])
    )


def test_a_mismatched_quote_pair_is_an_error_not_a_guess():
    """Opened with one style and closed with the other: obviously a mistake, so it
    must not parse — and must be REPORTED rather than dropped."""
    text = "<|tool_call>call:a.b{k: 'value\"}<tool_call|>"
    assert extract_tool_calls(text) == []
    assert len(malformed_calls(text)) == 1


def test_quoted_channel_strings_may_contain_structural_characters():
    """The bare-token reader stops at `,}]`; a quoted string must not.
    (`call:a.b` is DOTTED, so `b` becomes the payload's verb — hence `type`.)"""
    text = "<|tool_call>call:a.b{k: 'a,b}c[d', j: 2}<tool_call|>"
    args = extract_tool_calls(text)[0]["arguments"]
    assert args["payload"] == {"type": "b", "k": "a,b}c[d", "j": 2}


def test_the_other_quote_style_survives_inside_a_string():
    text = "<|tool_call>call:a.b{k: 'it\"s', m: \"it's\"}<tool_call|>"
    payload = extract_tool_calls(text)[0]["arguments"]["payload"]
    assert (payload["k"], payload["m"]) == ('it"s', "it's")


def test_escapes_decode_the_same_under_either_quote():
    """`\\n` is a newline and `\\\\` a single backslash under both styles; each
    style also escapes its OWN quote."""
    for body, want in (
        (r"{k: 'a\nb\\c\'d'}", "a\nb\\c'd"),
        (r'{k: "a\nb\\c\"d"}', 'a\nb\\c"d'),
    ):
        call = extract_tool_calls(f"<|tool_call>call:a.b{body}<tool_call|>")[0]
        assert call["arguments"]["payload"]["k"] == want, body


def test_an_unknown_escape_survives_verbatim():
    """A Windows path in a program body must not be silently mangled."""
    call = extract_tool_calls(r"<|tool_call>call:a.b{k: 'C:\dir'}<tool_call|>")[0]
    assert call["arguments"]["payload"]["k"] == r"C:\dir"


def test_a_quoted_key_in_either_style_loses_its_quotes():
    """`"id"` as a literal key name is what killed three posture changes on
    2026-08-04; single quotes must not reintroduce it."""
    for body in ("{'id': 'assets'}", '{"id": "assets"}'):
        args = extract_tool_calls(f"<|tool_call>call:a.update{body}<tool_call|>")[0]
        assert args["arguments"]["payload"]["id"] == "assets", body


def test_the_json_envelope_verb_in_the_tool_name_becomes_the_payload():
    """VERBATIM from gemma-4-12B, probe 05 run 3, 2026-08-06. `send` is the only
    tool, so any other `name` is verb+tool compressed — the same compression
    `call:<agent>.<verb>` gets, in the JSON envelope. Untranslated it dispatched
    `send("clock", None)` and raised `'NoneType' object has no attribute 'get'`."""
    call = extract_tool_calls(
        '<tool_call>{"name":"reflect","arguments":{"target_id":"clock"}}</tool_call>'
    )[0]
    assert call["name"] == "send"
    assert call["arguments"] == {"target_id": "clock", "payload": {"type": "reflect"}}


def test_a_named_verb_keeps_its_other_arguments_in_the_payload():
    call = extract_tool_calls(
        '<tool_call>{"name":"write","arguments":{"target_id":"a","path":"f.txt",'
        '"reply_to":"self"}}</tool_call>'
    )[0]
    assert call["arguments"]["payload"] == {"type": "write", "path": "f.txt"}
    assert call["arguments"]["reply_to"] == "self", "reply_to is routing, not payload"


def test_an_explicit_payload_beats_the_tool_name():
    call = extract_tool_calls(
        '<tool_call>{"name":"reflect","arguments":{"target_id":"a",'
        '"payload":{"type":"list"}}}</tool_call>'
    )[0]
    assert call["arguments"]["payload"] == {"type": "list"}


def test_a_named_verb_without_a_target_is_left_alone():
    """`{"name":"list_agents","arguments":{}}` names no agent. We do not guess one —
    it earns the `no target_id` error, which teaches."""
    call = extract_tool_calls(
        '<tool_call>{"name":"list_agents","arguments":{}}</tool_call>'
    )[0]
    assert "target_id" not in call["arguments"]


def test_an_inline_payload_with_target_id_is_a_send():
    """Measured 2026-08-06: `call:kernel_state{type:'reflect', target_id:'readings'}`
    means "reflect readings". The verb was previously looked for ONLY in a dotted
    name, so this complete instruction was discarded."""
    call = extract_tool_calls(
        "<|tool_call>call:kernel_state{type: 'reflect', target_id: 'readings'}"
        "<tool_call|>"
    )[0]
    assert call["arguments"] == {
        "target_id": "readings",
        "payload": {"type": "reflect"},
    }


def test_an_undotted_name_with_an_inline_type_targets_the_name():
    """`call:kernel_state{type:'reflect'}` is `send('kernel_state', {…})`. It used
    to arrive with no `target_id` and earn an error for a clear request."""
    call = extract_tool_calls(
        "<|tool_call>call:kernel_state{type:'reflect'}<tool_call|>"
    )[0]
    assert call["name"] == "send"
    assert call["arguments"] == {
        "target_id": "kernel_state",
        "payload": {"type": "reflect"},
    }


def test_reply_to_stays_routing_and_never_enters_the_payload():
    call = extract_tool_calls(
        "<|tool_call>call:totals{type: 'set', key: 'k', reply_to: 'self'}<tool_call|>"
    )[0]
    assert call["arguments"]["reply_to"] == "self"
    assert call["arguments"]["payload"] == {"type": "set", "key": "k"}


def test_an_explicit_payload_still_wins_over_an_inline_type():
    """The 2026-08-04 shape must not regress: args carrying both `target_id` and
    `payload` pass through verbatim, stray top-level `type` and all."""
    call = extract_tool_calls(
        '<|tool_call>call:kernel_state.send{"payload":{"type":"reflect"},'
        '"target_id":"kernel_state"}<tool_call|>'
    )[0]
    assert call["arguments"]["target_id"] == "kernel_state"
    assert call["arguments"]["payload"] == {"type": "reflect"}


def test_the_channel_error_does_not_lecture_about_json():
    """Its body is not JSON. Advice that does not match the grammar the model used
    teaches the wrong lesson — measured live on 2026-08-06."""
    bad = malformed_calls("<|tool_call>call:a.b{k: 'unclosed}<tool_call|>")[0]
    assert "NOT JSON" in bad["error"]
    assert "\\n" not in bad["error"], "the JSON escaping hint must not appear here"


def test_the_broken_block_is_shown_whole():
    """No cap. It was 400 chars, which hid the break: in a long payload the fault is
    at the END (unterminated string, bad escape), so a prefix that looks fine tells
    the model nothing. Measured 2026-08-24 (`live_build`): three identical
    unparseable retries of a `create_agent` carrying 1.5k of `code`."""
    tail = 'print("done")'
    text = '<tool_call>{"name":"send","arguments":{"code":"' + "x" * 40000 + tail
    blk = malformed_calls(text)[0]["your_block"]
    assert blk.endswith(tail), "the tail is where the break is"
    assert len(blk) > 40000, "shown whole, not capped"


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))


# ─── _route_reply's reply tag (end-to-end through a stub kernel) ─


async def _settle(k, want: int, timeout: float = 2.0) -> None:
    """Wait for the spawned deliveries.

    `_route_reply` no longer AWAITS the turns its replies start — that is the fix
    for the 26-of-36 reply loss measured 2026-08-24. A caller that wants to observe
    a delivery therefore yields to the loop first."""
    began = asyncio.get_event_loop().time()
    while len(k.delivered) < want:
        if asyncio.get_event_loop().time() - began > timeout:
            break
        await asyncio.sleep(0.005)


async def test_reply_tag_names_pid_target_and_verb():
    """`[reply to <pid> · <id> <verb>]` — the dispatch PID is the correlator.

    Measured 2026-08-07 (opus): acks lag a turn behind when several calls are in
    flight, and a bare `[reply to kernel_state]` cannot be matched to WHICH call it
    answers — the model re-issued both deletes, then theorized "duplicate delivery"
    from its own retries. Target+verb narrowed it but is still NOT unique (two
    identical calls in flight); measured again 2026-08-20 (the model: "the reply queue is
    running ahead of me", then re-polled forever). The PID — minted at dispatch, the
    same id `ps`/`cancel` use — is the one unambiguous correlation.
    """
    from ai_core import core

    sent = []

    class StubKernel:
        async def send(self, items):
            outs = []
            for it in items:
                target, payload = it["target"], it["payload"]
                sent.append((target, payload))
                result = (
                    {"agents": []}
                    if payload.get("type") == "list_agents"
                    else None  # the reply-delivery self-send
                )
                # the kernel mints the pid at dispatch
                outs.append(
                    {"pid": f"{target}:1", "status": "completed", "result": result}
                )
            return outs

    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"kernel_state",'
        '"payload":{"type":"list_agents"},"reply_to":"self"}}</tool_call>'
    )
    await core._route_reply("ai", StubKernel(), text, "cli", None, None, 180.0, "")
    # Deliveries are spawned, not awaited (see `_deliver_soon`) — yield to the loop.
    for _ in range(200):
        if [p for t, p in sent if t == "ai"]:
            break
        await asyncio.sleep(0.005)
    dispatched = [p for t, p in sent if t == "kernel_state"]
    assert dispatched == [{"type": "list_agents"}], "exactly ONE dispatch per call"
    back = [p for t, p in sent if t == "ai"]
    assert len(back) == 1
    assert back[0]["text"].startswith(
        "[reply to kernel_state:1 · kernel_state list_agents]\n"
    )
