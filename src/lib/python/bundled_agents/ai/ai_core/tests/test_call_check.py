"""The CALL CHECKER: markup carrying our call vocabulary outside
every `<call` envelope is a call written wrong, reported like a compiler error — the
line, a caret, and the form to write instead, built from what the model wrote. The
answer's well-formed calls still run, and the report says so.

The first shape is one a real model wrote repeatedly; the rest are the syntaxes other harnesses meet — a call name as a tag, a
JSON call, Qwen3-Coder's `<function=…><parameter=…>`."""

from _testkit import memory_meta
import asyncio

from ai_core import core
from ai_core.call_check import MAX_REPORTS, check, repair

E59 = """I'll look at the files agent.
<tool_call>
<function=call name="send">
<target_id>files</target_id>
<type>reflect</type>
</call>
</tool_call>"""

TAIL = 'A call is <call name="…">, one child tag per argument, closed by </call>.'


# ─── the reports ─────────────────────────────────────────────────────────────


def test_the_measured_shape_is_reported_with_the_form_to_write():
    assert check(E59, ran=0) == [
        "[malformed call — nothing ran] line 3, col 1:\n"
        '  <function=call name="send">\n'
        "  ^~~~~~~~~~~~~~~~~~~~~~~~~~~ not a call tag\n"
        "write it as:\n"
        '<call name="send">\n'
        "<target_id>files</target_id>\n"
        "<type>reflect</type>\n"
        "</call>\n"
        "also: line 2 `<tool_call>` — drop the wrapper.\n" + TAIL
    ]


def test_a_call_name_as_a_tag_next_to_a_good_call_says_the_good_one_ran():
    answer = (
        '<call name="send"><target_id>a</target_id><type>reflect</type></call>\n'
        "<send><target_id>b</target_id><type>read</type></send>"
    )
    (r,) = check(answer, ran=1)
    assert r == (
        "[malformed call — nothing ran] line 2, col 1:\n"
        "  <send><target_id>b</target_id><type>read</type></send>\n"
        "  ^~~~~~ not a call tag\n"
        "write it as:\n"
        '<call name="send">\n'
        "<target_id>b</target_id>\n"
        "<type>read</type>\n"
        "</call>\n"
        "also: `</send>` — close with </call>.\n" + TAIL + "\n"
        "The other call of this answer ran — its reply is in this batch; do not "
        "write it again."
    )
    (r,) = check(answer, ran=2)
    assert r.endswith(
        "The other 2 calls of this answer ran — their replies are in this batch; "
        "do not write them again."
    )


def test_a_json_call_is_shown_as_tags():
    (r,) = check(
        '{"name": "send", "arguments": {"target_id": "files", '
        '"payload": {"type": "list", "path": "."}}}',
        ran=0,
    )
    assert "^~~~~~~~~~~~~~~ calls are tags, not JSON\n" in r, r
    assert (
        'write it as:\n<call name="send">\n<target_id>files</target_id>\n'
        "<type>list</type>\n<path>.</path>\n</call>\n"
    ) in r, r


def test_qwen_coder_parameters_become_child_tags():
    (r,) = check(
        "<function=send>\n<parameter=target_id>\nfiles\n</parameter>\n"
        "<parameter=type>reflect</parameter>\n</function>",
        ran=0,
    )
    assert r.startswith(
        "[malformed call — nothing ran] line 1, col 1:\n  <function=send>\n"
        "  ^~~~~~~~~~~~~~~ not a call tag\n"
    ), r
    assert '<call name="send">\n<target_id>files</target_id>\n<type>reflect</type>' in r
    assert (
        "also: `<parameter=target_id>` — one child tag per argument: "
        "<target_id>…</target_id>; `<parameter=type>` — one child tag per argument: "
        "<type>…</type>."
    ) in r, r


def test_a_control_call_as_a_bare_tag():
    (r,) = check("finished.\n<done/>", ran=0)
    assert "line 2, col 1:\n  <done/>\n  ^~~~~~~ not a call tag\n" in r, r
    assert 'write it as:\n<call name="done"></call>\n' in r, r


def test_arguments_with_no_call_open():
    (r,) = check("<target_id>notes</target_id>\n<type>read</type>", ran=0)
    assert "^~~~~~~~~~~ an argument outside a call — open the call above it" in r, r
    assert '<call name="send">\n<target_id>notes</target_id>\n<type>read</type>' in r


def test_the_verbs_own_arguments_carry_into_the_form_to_write():
    """A grep scoped by `<ids>` that lost its leading `<` must not be reported with
    only target/title/type — the model would rewrite it unscoped. Every argument the model wrote carries over, a nested
    `<item>` as part of its list."""
    broken = """call name="send">
<target_id>ai_fold</target_id>
<title>grep for checksum</title>
<type>grep</type>
<query>checksum</query>
<ids><item>32</item></ids>
</call>"""
    (r,) = check(broken, ran=0)
    form = r.split("write it as:\n", 1)[1].split("\nA call is", 1)[0]
    assert form == (
        '<call name="send">\n<target_id>ai_fold</target_id>\n'
        "<title>grep for checksum</title>\n<type>grep</type>\n"
        "<query>checksum</query>\n<ids><item>32</item></ids>\n</call>"
    ), r


def test_a_call_opener_that_lost_its_lt_is_named_as_that():
    """An answer after `</think>` of exactly `call name="await"></call>` — the
    opening `<` dropped. Only the `</call>` is a tag, so the report would say "a
    close with no call open — delete it"; it names the missing `<` and the call to
    write."""
    (r,) = check('call name="await"></call>', ran=0)
    assert r == (
        "[malformed call — nothing ran] line 1, col 1:\n"
        '  call name="await">‹/call›\n'
        "  ^~~~~~~~~~~~~~~~~~ the opening `<` is missing\n"
        "write it as:\n"
        '<call name="await"></call>\n' + TAIL
    ), r


def test_a_stray_close_after_a_good_call():
    answer = '<call name="done"></call></call>'
    (r,) = check(answer, ran=0)
    assert "line 1, col 26:" in r, r
    # the quote is defanged — it cannot arm the scanner when the model reads it
    assert '‹call name="done">‹/call›‹/call›' in r, r
    assert r.split("\n")[2].endswith("^~~~~~~ a close with no call open — delete it"), r
    assert "write it as" not in r, r


def test_prose_and_foreign_markup_without_our_vocabulary_are_not_calls():
    for answer in [
        "All done, the file is written.",
        "Use <b>bold</b> and a <div class='x'>box</div>; 3 < 4 > 2.",
        "<tool_call>\n</tool_call>",
        '<call name="done"></call>',
    ]:
        assert check(answer, ran=0) == [], answer


def test_what_a_call_envelope_holds_is_never_checked():
    # inside a good call's bytes, and inside an envelope the scanner reports itself
    good = (
        '<call name="send"><target_id>notes</target_id><type>write</type>'
        '<path>x.md</path><bytes><send>no</send> </call> {"name": "send"}</bytes>'
        "</call>"
    )
    assert check(good, ran=1) == []
    assert check('<call name="send" target_id="x"></call>', ran=0) == []


def test_reports_are_capped():
    answer = "\n".join(f"<send><target_id>a{i}</target_id></send>" for i in range(5))
    reports = check(answer, ran=0)
    assert len(reports) == MAX_REPORTS
    assert reports[-1].endswith("(2 more malformed calls not shown.)"), reports[-1]


def test_a_long_line_is_windowed_and_the_caret_follows():
    answer = "x" * 100 + '<function=call name="send"></call>'
    (r,) = check(answer, ran=0)
    quoted, caret = r.split("\n")[1:3]
    assert quoted.startswith("  …") and caret.lstrip().startswith("^")
    assert quoted.index("<function") == caret.index("^"), (quoted, caret)


# ─── through the agent ───────────────────────────────────────────────────────


class _Script:
    def __init__(self, turns):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        yield self.turns.pop(0) if self.turns else "…nothing left…"


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, fp.incoming
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.15)


def _agent(kernel, aid):
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*", "kernel": "*"}),
    )


async def test_a_broken_call_is_reported_instead_of_the_no_call_reminder(
    seeded_kernel, fake_providers
):
    fp = _Script([E59, '<call name="done"></call>'])
    aid = "cc_a"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)
        nxt = fp.incoming[1]
        assert "[no call was dispatched]" not in nxt, nxt
        assert "[malformed call — nothing ran] line 3, col 1:" in nxt, nxt
        assert '<call name="send">\n<target_id>files</target_id>' in nxt, nxt
    finally:
        fake_providers.pop(aid, None)


WRAPPED = '<tool_call>\n<function=call name="done"></call>\n</tool_call>'
REFLECT = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'


async def test_a_second_wrapped_answer_in_a_row_escalates(
    seeded_kernel, fake_providers
):
    fp = _Script([WRAPPED, WRAPPED, '<call name="done"></call>'])
    aid = "cc_esc"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 3)
        assert "[no progress" not in fp.incoming[1], fp.incoming[1]
        assert "[no progress — 2 answers in a row ran nothing]" in fp.incoming[2], (
            fp.incoming[2]
        )
        assert "<tool_call>" in fp.incoming[2], fp.incoming[2]
        assert "must START with <call" in fp.incoming[2], fp.incoming[2]
    finally:
        fake_providers.pop(aid, None)


async def test_a_call_that_runs_resets_the_escalation(seeded_kernel, fake_providers):
    fp = _Script([WRAPPED, REFLECT, WRAPPED, '<call name="done"></call>'])
    aid = "cc_reset"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 4)
        assert "[no progress" not in fp.incoming[3], fp.incoming[3]
    finally:
        fake_providers.pop(aid, None)


async def test_the_good_calls_run_and_the_broken_one_comes_back_beside_them(
    seeded_kernel, fake_providers
):
    good = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'
    fp = _Script(
        [
            good + "\n<send><target_id>kernel</target_id><type>ps</type></send>",
            '<call name="done"></call>',
        ]
    )
    aid = "cc_b"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)
        batch = fp.incoming[1]
        # one batch: the reflect's reply AND the report
        assert "kernel reflect" in batch, batch
        assert "[malformed call — nothing ran] line 2, col 1:" in batch, batch
        assert "The other call of this answer ran" in batch, batch
    finally:
        fake_providers.pop(aid, None)


async def test_broken_only_turns_count_toward_giving_up(
    seeded_kernel, monkeypatch, fake_providers
):
    """A model stuck on one broken shape still ends: a turn whose only output is
    broken calls ran nothing, and counts as silent."""
    monkeypatch.setattr(core, "MAX_SILENT_TURNS", 3)
    fp = _Script([E59] * 10)
    aid = "cc_c"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 3)
        await asyncio.sleep(0.5)
        assert fp.calls == 3, fp.incoming
    finally:
        fake_providers.pop(aid, None)


def test_a_multibyte_line_keeps_the_caret_under_the_token():
    (r,) = check("é→ <send><target_id>a</target_id></send>", ran=0)
    assert r.startswith("[malformed call — nothing ran] line 1, col 4:\n  é→ <send>"), r
    assert "\n     ^~~~~~ not a call tag" in r, r


def test_repair_gives_a_dropped_lt_back_and_nothing_else():
    """The spell checker: vLLM's streaming reasoning parser drops
    what shares a delta with `</think>` — the `<` of the call after it. A bare
    `call name="X">` closed by `</call>` gets its `<` back; prose, a well-formed call
    and an opener nothing closes stay as written."""
    broken = (
        'call name="send">\n<target_id>kernel</target_id>\n<type>docs</type>\n</call>'
    )
    assert repair(broken) == "<" + broken
    assert check(repair(broken), ran=1) == []
    for same in [
        'I will call name="x"> later',
        '<call name="done"></call>',
        'call name="send"><target_id>a</target_id>',
    ]:
        assert repair(same) == same


async def test_a_call_that_lost_its_lt_runs_and_is_stored_whole(
    seeded_kernel, fake_providers
):
    """The answer after `</think>` is the call
    without its `<`. It RUNS — its reply is the next turn, no malformed report — and
    the stored answer is the repaired call."""
    from ai_core import core as _core

    fp = _Script(
        [
            '<think>x</think>call name="send"><target_id>kernel</target_id>'
            "<type>reflect</type></call>",
            '<call name="done"></call>',
        ]
    )
    aid = "cc_lt"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)
        nxt = fp.incoming[1]
        assert "[malformed call" not in nxt and "[reply to " in nxt, nxt
        assert "kernel reflect" in nxt, nxt
        rows = await _core._load_history(aid, seeded_kernel)
        said = [r["content"] for r in rows if r.get("role") == "assistant"]
        assert said[0].startswith('<call name="send">'), said
    finally:
        fake_providers.pop(aid, None)
