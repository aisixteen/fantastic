"""#274: the no-progress escalation says what to do, never the opposite.

Python twin of the Rust test `escalation_texts_say_what_to_do` (#276)."""

from ai_core.core import _escalation


def test_escalation_texts_say_what_to_do():
    done = '<call name="done"></call>'
    d = _escalation(3, "no_call")
    assert d.startswith(
        "[no progress — 3 answers in a row ran nothing] Your last 3 answers "
        "had no call, so nothing ran.\n"
    )
    assert f"If the task is finished, that call is {done}." in d
    assert "nowhere" not in d

    t = _escalation(2, "thinking_only")
    assert "Keep your thinking to a few lines" in t
    assert "nowhere" not in t

    w = _escalation(2, "wrapped", "tool_call")
    assert "and contain <tool_call> nowhere." in w

    m = _escalation(2, "malformed")
    assert "with no other markup around or inside the call." in m
    assert "nowhere" not in m

    for s in (d, t, w, m):
        assert s.endswith("\nThen continue with your current step.")


def test_escalation_texts_match_rust():
    assert _escalation(3, "no_call") == (
        "[no progress — 3 answers in a row ran nothing] Your last 3 answers had no call, so nothing ran.\n"
        'Your next answer must START with <call name="…"> — the very first characters of the answer. '
        'If the task is finished, that call is <call name="done"></call>.\n'
        "Then continue with your current step."
    )
