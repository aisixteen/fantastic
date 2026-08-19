"""proc — THE spawn primitive: the allowlist, its resolution, and the argv shape.

Mirrors `file_bridge`'s `test_fs.py`: the LAW is pinned on the primitive itself,
because the primitive is the only surface that can spawn — there is no second path a
test would have to cover. Pure functions; no process is started in this file.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from exec_bridge import proc

# ─── (a) no default interpreter ─────────────────────────────────


def test_an_absent_allowlist_spawns_nothing():
    """An unwired leg must resolve to NOTHING, not to a guessed interpreter.

    `python_runtime` could fall back to `sys.executable` because the host IS python —
    a fact about the running process. This bridge spawns whatever the operator listed,
    so a default here would be a guess that grants local code execution."""
    assert proc.allowed({}) == []
    assert proc.allowed({"interpreters": []}) == []
    assert proc.allowed({"interpreters": ["", "   "]}) == []


def test_an_empty_allowlist_refuses_and_names_who_must_fix_it():
    """A refusal that does not say what WOULD work only teaches that something is
    wrong. The empty-allowlist message must name the FIELD and the OPERATOR, or a
    caller (human or model) has nowhere to go from the error."""
    with pytest.raises(proc.InterpreterRefused) as e:
        proc.resolve({}, None)
    assert "operator must set" in str(e.value)
    assert "interpreters=" in str(e.value)


def test_a_request_cannot_open_an_empty_allowlist():
    """Naming an interpreter per call must NOT be a way past an unset allowlist —
    that is the per-call escape hatch `proc`'s docstring refuses to have."""
    with pytest.raises(proc.InterpreterRefused) as e:
        proc.resolve({}, "/usr/bin/python3")
    assert "operator must set" in str(e.value)


# ─── (b) one entry vs many ──────────────────────────────────────


def test_one_allowlisted_entry_may_be_left_unnamed():
    """With exactly one legal answer there is no ambiguity to resolve, so demanding
    the caller restate it would be ceremony — an empty/absent `interpreter` resolves."""
    one = {"interpreters": ["/usr/bin/python3"]}
    assert proc.resolve(one, None) == "/usr/bin/python3"
    assert proc.resolve(one, "") == "/usr/bin/python3"
    assert proc.resolve(one, "   ") == "/usr/bin/python3"


def test_two_allowlisted_entries_must_be_chosen_between():
    """Silence with two legal answers is AMBIGUOUS; picking one would be a guess
    about which environment (which installed dependencies) the caller meant."""
    two = {"interpreters": ["/usr/bin/python3", "/opt/py/bin/python"]}
    with pytest.raises(proc.InterpreterRefused) as e:
        proc.resolve(two, None)
    assert "name one" in str(e.value)
    assert "/usr/bin/python3" in str(e.value) and "/opt/py/bin/python" in str(e.value)
    # …and naming one of them resolves.
    assert proc.resolve(two, "/opt/py/bin/python") == "/opt/py/bin/python"


def test_a_non_allowlisted_request_is_refused_naming_the_legal_set():
    """The refusal carries the whole allowlist: that is the mechanism measured to let
    a small model recover on its very next turn instead of guessing again."""
    with pytest.raises(proc.InterpreterRefused) as e:
        proc.resolve({"interpreters": ["/usr/bin/python3"]}, "/bin/sh")
    assert "/bin/sh" in str(e.value), "the refusal must name what was asked for"
    assert "/usr/bin/python3" in str(e.value), "…and what would have worked"


# ─── (c) how an entry is written ────────────────────────────────


def test_a_bare_string_is_one_entry_not_a_list_of_characters():
    """One interpreter written bare (`interpreters="/usr/bin/python3"`) is the shape
    an operator/model actually types; iterating the string would allowlist letters."""
    assert proc.allowed({"interpreters": "/usr/bin/python3"}) == ["/usr/bin/python3"]
    assert (
        proc.resolve({"interpreters": "/usr/bin/python3"}, None) == "/usr/bin/python3"
    )


def test_a_tilde_entry_and_its_expansion_are_the_same_entry():
    """`~/.venvs/x/bin/python` is how a venv is written down; without expansion the
    same interpreter would be two different allowlist entries and a leg listed with
    `~` would refuse the very path it names."""
    home_py = str(Path.home() / ".venvs" / "x" / "bin" / "python")
    tilde = "~/.venvs/x/bin/python"
    assert proc.allowed({"interpreters": [tilde]}) == [home_py]
    # listed with `~`, requested expanded
    assert proc.resolve({"interpreters": [tilde]}, home_py) == home_py
    # listed expanded, requested with `~`
    assert proc.resolve({"interpreters": [home_py]}, tilde) == home_py


# ─── (d) argv is data, not a python assumption ──────────────────


def test_argv_is_data_so_the_bridge_need_not_know_the_language():
    """`-u -c` is CPython-specific. If the shape were hardcoded, a leg allowlisting
    node/ruby could never run — the bundle would silently be python-only again, which
    is the coupling this bridge exists to remove."""
    assert proc.argv({}, "/p", "CODE") == ["/p", "-u", "-c", "CODE"]
    node = {"argv_template": ["<interp>", "-e", "<program>"]}
    assert proc.argv(node, "/usr/bin/node", "CODE") == ["/usr/bin/node", "-e", "CODE"]
    # a bare-string template is one argv entry, and non-placeholder args pass through
    assert proc.argv({"argv_template": "<interp>"}, "/p", "CODE") == ["/p"]
    keep = {"argv_template": ["<interp>", "--flag", "<program>"]}
    assert proc.argv(keep, "/p", "CODE") == ["/p", "--flag", "CODE"]
