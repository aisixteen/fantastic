"""exec_bridge — the IO auth gate + the ALLOWLIST law (the two things that seal it).

The process boundary is an IO edge, so this mirrors `file_bridge`'s gate suite: a leg
with no `ingress_rule` is SEALED (every verb except `reflect` denies with the teaching
shape), and even an OPEN leg spawns nothing but an interpreter the operator listed in
`interpreters` — refused at the WRITE (next to the typo) and refused again at `start`,
with no per-call escape hatch.
"""

from __future__ import annotations

import os
import sys

import pytest
from exec_bridge.tools import UNGATED, VERBS


def _real_python() -> str:
    """A real interpreter for the TEST to wire onto a record's allowlist.

    `sys.executable` is the operator's choice here — which is exactly what the
    allowlist models. Skip cleanly rather than assert about a machine that has none."""
    if not (sys.executable and os.access(sys.executable, os.X_OK)):
        pytest.skip("no executable interpreter available to allowlist")
    return sys.executable


async def _make(kernel, **meta) -> str:
    rec = await kernel.send(
        kernel.id,
        {"type": "create_agent", "handler_module": "exec_bridge.tools", **meta},
    )
    assert "id" in rec, f"exec_bridge creation failed: {rec!r}"
    return rec["id"]


# ─── (a) sealed by default ──────────────────────────────────────


async def test_every_verb_but_reflect_denies_on_a_sealed_leg(seeded_kernel):
    """A freshly created leg must spawn NOTHING until an operator opens it.

    Derived from the VERBS table rather than a hand-written list, so a verb added
    later without a gate decision fails here instead of shipping an ungated process
    edge — the one boundary in the previous executor that was not sealed."""
    xid = await _make(seeded_kernel)  # NO ingress_rule — the seal
    assert UNGATED == {"reflect"}, "only discovery may answer through the seal"
    for verb in sorted(set(VERBS) - UNGATED):
        r = await seeded_kernel.send(xid, {"type": verb, "code": "print(1)"})
        assert r["reason"] == "unauthorized", f"{verb} answered on a sealed leg: {r!r}"
        assert "error" in r
        assert "ingress_rule" in r["hint"], "the denial must teach how to open"
        assert "job_id" not in r, f"{verb} spawned something through the seal: {r!r}"


async def test_a_sealed_leg_still_reflects(seeded_kernel):
    """Discovery must work through the seal — otherwise a denied caller cannot learn
    what the leg is, what it would run, or how to open it, and dead-ends."""
    xid = await _make(seeded_kernel, interpreters=[_real_python()])
    r = await seeded_kernel.send(xid, {"type": "reflect"})
    assert r["sealed"] is True
    assert "verbs" in r
    assert r["interpreters"] == [_real_python()], "the allowlist is advertised sealed"
    assert r["shims"] == ["python"]


async def test_allow_all_opens_the_leg(seeded_kernel):
    """The seal is a POSTURE, not a wall: the identical verbs answer once the operator
    sets `ingress_rule=allow_all`."""
    xid = await _make(
        seeded_kernel, ingress_rule="allow_all", interpreters=[_real_python()]
    )
    assert await seeded_kernel.send(xid, {"type": "status"}) == {"jobs": []}
    assert await seeded_kernel.send(xid, {"type": "clear"}) == {"cleared": 0}
    assert await seeded_kernel.send(xid, {"type": "stop"}) == {"killed": 0}


async def test_reflect_surfaces_the_posture_both_ways(seeded_kernel):
    """`reflect` is how an operator reads which legs are open — keyed by the field you
    SET, so `ingress_rule: deny_inbound` is opened with `ingress_rule: allow_all`."""
    sealed = await _make(seeded_kernel)
    r = await seeded_kernel.send(sealed, {"type": "reflect"})
    assert r["ingress_rule"] == "deny_inbound" and r["sealed"] is True

    opened = await _make(seeded_kernel, ingress_rule="allow_all")
    r = await seeded_kernel.send(opened, {"type": "reflect"})
    assert r["ingress_rule"] == "allow_all" and r["sealed"] is False


async def test_reflect_never_raises_on_an_unwired_leg(seeded_kernel):
    """Reflect is the ONE verb that must answer no matter how broken the wiring is —
    it is where a caller goes to find out that the wiring is broken. A leg with
    nothing allowlisted reports an empty set instead of raising or inventing one."""
    xid = await _make(seeded_kernel)
    r = await seeded_kernel.send(xid, {"type": "reflect"})
    assert r["interpreters"] == []
    assert r["running"] == 0
    assert r["cwd"] == "<process default>"
    assert "start" in r["verbs"] and "job_done" in r["emits"]


async def test_an_unimplementable_ingress_rule_is_refused_at_the_write(seeded_kernel):
    """Accepting a rule you cannot honour and reporting success is the worst of both:
    the leg boots believing it has a posture it does not have. Refuse at create AND
    update, naming the legal values (measured 2026-08-06: gemma-4-12B recovered on its
    next turn from exactly this message)."""
    r = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "exec_bridge.tools",
            "ingress_rule": "deny_all",
        },
    )
    assert "error" in r and "legal values" in r["error"]
    assert r.get("created") is False and "id" not in r

    good = await _make(seeded_kernel, ingress_rule="allow_all")
    u = await seeded_kernel.send(
        "kernel_state",
        {"type": "update_agent", "id": good, "ingress_rule": "deny_all"},
    )
    assert u.get("updated") is False and "legal values" in u["error"]


# ─── (b) the allowlist, refused at the WRITE ────────────────────


async def test_a_non_allowlisted_interpreter_is_refused_at_CREATE(seeded_kernel):
    """`create_agent` discards the boot REPLY, so a record naming an interpreter the
    leg does not allow would look created, fail to spawn, and be indistinguishable
    from a program that ran and did nothing. The write is where it must fail."""
    py = _real_python()
    r = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "exec_bridge.tools",
            "ingress_rule": "allow_all",
            "interpreters": [py],
            "interpreter": "/nowhere/bin/python",
            "code": "print('must never run')",
        },
    )
    assert "error" in r, f"a non-allowlisted interpreter must refuse, got {r!r}"
    assert "not allowlisted" in r["error"]
    assert py in r["error"], "the refusal must NAME the allowed set"
    assert r.get("created") is False and "id" not in r, "nothing may be created"


async def test_a_non_allowlisted_interpreter_is_refused_at_UPDATE(seeded_kernel):
    """The same function must guard the other write verb, or an `update_agent` is a
    way to smuggle an interpreter past the allowlist after creation.

    NOTE the patch restates `interpreters`: `validate_meta` resolves `interpreter`
    against the meta IT IS GIVEN, so an operator rewiring both fields validates as one
    unit regardless of the order they are written in."""
    py = _real_python()
    xid = await _make(
        seeded_kernel, ingress_rule="allow_all", interpreters=[py], interpreter=py
    )
    u = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "update_agent",
            "id": xid,
            "interpreters": [py],
            "interpreter": "/nowhere/bin/python",
        },
    )
    assert u.get("updated") is False, f"update must refuse, got {u!r}"
    assert "not allowlisted" in u["error"] and py in u["error"]
    # and the refused patch did not half-apply
    assert (seeded_kernel.get(xid) or {})["interpreter"] == py


async def test_an_allowlisted_interpreter_still_writes(seeded_kernel):
    """The refusal must not have made the write paranoid — the happy path is
    untouched, and the record keeps the pair the operator chose."""
    py = _real_python()
    xid = await _make(
        seeded_kernel, ingress_rule="allow_all", interpreters=[py], interpreter=py
    )
    r = await seeded_kernel.send(xid, {"type": "reflect"})
    assert r["interpreters"] == [py] and r["sealed"] is False


async def test_interpreters_must_be_a_list_of_paths(seeded_kernel):
    """A malformed allowlist must be refused at the write, or `reflect` (which reads
    it on every call) is the thing that breaks, far from the mistake."""
    for bad in (5, {"a": 1}, ["/usr/bin/python3", "  "]):
        r = await seeded_kernel.send(
            "kernel_state",
            {
                "type": "create_agent",
                "handler_module": "exec_bridge.tools",
                "interpreters": bad,
            },
        )
        assert "error" in r, f"interpreters={bad!r} was accepted: {r!r}"
        assert "list of interpreter paths" in r["error"]
        assert r.get("created") is False

    # …and the shape an operator actually types — one interpreter, written bare.
    xid = await _make(seeded_kernel, interpreters=_real_python())
    got = await seeded_kernel.send(xid, {"type": "reflect"})
    assert got["interpreters"] == [_real_python()]


# ─── (c) the allowlist, refused again at START ──────────────────


async def test_an_empty_allowlist_refuses_start_and_names_the_operator(seeded_kernel):
    """An OPEN leg with nothing allowlisted is a legal, meaningful state — wired but
    given nothing to run. It must refuse every `start` and say who fixes it, rather
        than guess an interpreter (the guess has no honest answer)."""
    for meta in ({}, {"interpreters": []}):
        xid = await _make(seeded_kernel, ingress_rule="allow_all", **meta)
        r = await seeded_kernel.send(xid, {"type": "start", "code": "print(1)"})
        assert "error" in r, f"interpreters={meta} spawned something: {r!r}"
        assert "no interpreters are allowlisted" in r["error"]
        assert "operator must set" in r["error"]
        assert "job_id" not in r, "nothing may be spawned"
        assert await seeded_kernel.send(xid, {"type": "status"}) == {"jobs": []}


async def test_one_allowlisted_interpreter_may_be_left_unnamed_on_start(seeded_kernel):
    """With exactly one legal answer, `start` without `interpreter` must run — making
    the caller restate the only possibility would be ceremony."""
    xid = await _make(
        seeded_kernel, ingress_rule="allow_all", interpreters=[_real_python()]
    )
    r = await seeded_kernel.send(
        xid, {"type": "start", "code": "import time\ntime.sleep(30)"}
    )
    assert r["status"] == "running" and r["job_id"], f"start refused: {r!r}"
    await seeded_kernel.send(xid, {"type": "stop"})


async def test_two_allowlisted_interpreters_must_be_named_on_start(
    seeded_kernel, tmp_path
):
    """Two legal answers make silence ambiguous: which environment (which installed
    dependencies) the caller meant is unknowable, so the bridge refuses instead of
    picking — and the refusal lists both so the caller can choose."""
    py = _real_python()
    other = str(tmp_path / "other_python")  # need not exist — nothing is spawned
    xid = await _make(seeded_kernel, ingress_rule="allow_all", interpreters=[py, other])
    r = await seeded_kernel.send(xid, {"type": "start", "code": "print(1)"})
    assert "error" in r, f"an ambiguous start was served: {r!r}"
    assert "name one" in r["error"]
    assert py in r["error"] and other in r["error"]
    assert "job_id" not in r

    # naming one of them runs it
    ok = await seeded_kernel.send(
        xid,
        {
            "type": "start",
            "code": "import time\ntime.sleep(30)",
            "interpreter": py,
        },
    )
    assert ok["status"] == "running" and ok["job_id"], f"named start refused: {ok!r}"
    await seeded_kernel.send(xid, {"type": "stop"})


async def test_a_per_call_interpreter_cannot_leave_the_allowlist(seeded_kernel):
    """The law is not overridable per call. If `start interpreter=…` could name
    anything, the allowlist would be a suggestion and the leg would grant arbitrary
    local code execution to whoever can reach it."""
    xid = await _make(
        seeded_kernel, ingress_rule="allow_all", interpreters=[_real_python()]
    )
    r = await seeded_kernel.send(
        xid,
        {"type": "start", "code": "print(1)", "interpreter": "/bin/sh"},
    )
    assert "error" in r and "not allowlisted" in r["error"]
    assert _real_python() in r["error"]
    assert "job_id" not in r
