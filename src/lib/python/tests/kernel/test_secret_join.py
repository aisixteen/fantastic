"""JOINING a secret-marked execution (audit 2026-10-03, F1).

A join hands back the execution's outcome without running anything, re-gated on
the ORIGINAL verb. Measured before the fix: `eve`, granted the same verb on a
credential_store but never a reader of the ref, joined `alice`'s marked `get` by
its int pid (and by its name) and received the real key — the store's per-ref
`readers` check runs in its handler, which a joiner never reaches, and the joiner
marked nothing.

The rule: an execution whose item carried `secret` marks hands its REAL outcome to
its ORIGINAL caller only — the identity that opened it: the sending agent, or the
operator (no sender), or, when a bridge leg relays a foreign caller, that caller's
qualified `<guid>|<agent>`. Every other joiner gets the outcome AS SHOWN — the
result marks and every secret value replaced by `<secret>`, exactly what the
ledger reads back. A join of an unmarked execution is unchanged."""

from __future__ import annotations

import asyncio
import types

from kernel import remote_caller_context, sender_context, register_unlisted
from kernel._secret import PLACEHOLDER

KEY = "sk-join-0123456789abcdef"

_release: asyncio.Event | None = None


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "join probe", "verbs": {}}
    if verb == "issue":
        return {"key": KEY, "note": "issued"}
    if verb == "slow_issue":
        await _release.wait()
        return {"key": KEY, "note": "issued"}
    if verb == "echo":
        return {"echo": payload.get("text")}
    return {"error": f"unknown verb {verb!r}"}


_mod = types.ModuleType("secret_join_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("secret_join_probe.tools", _mod)
MOD = "secret_join_probe.tools"


def _seed(k):
    k.create(
        MOD,
        id="vault",
        callable_by={"alice": "*", "eve": "*", "leg": "*", "P-g|*": "*"},
    )
    for aid in ("alice", "eve", "leg"):
        k.create(None, id=aid, can_call={"vault": "*"})


async def _send(k, item, sender=None, remote=None):
    if sender is None:
        return (await k.send([item]))[0]
    with sender_context(sender):
        if remote is None:
            return (await k.send([item]))[0]
        with remote_caller_context(sender, remote):
            return (await k.send([item]))[0]


def _issue(**extra):
    return {
        "target": "vault",
        "payload": {"type": "issue"},
        "secret": ["result.key"],
        **extra,
    }


def _shown(out):
    return out["status"] == "completed" and out["result"] == {
        "key": PLACEHOLDER,
        "note": "issued",
    }


def _real(out):
    return out["status"] == "completed" and out["result"] == {
        "key": KEY,
        "note": "issued",
    }


async def test_a_non_originating_joiner_gets_the_shown_outcome_by_pid(kernel):
    k = kernel
    _seed(k)
    own = await _send(k, _issue(), "alice")
    assert _real(own), own
    joined = await _send(
        k, {"target": "vault", "id": own["pid"], "payload": {"type": "issue"}}, "eve"
    )
    assert _shown(joined), joined
    assert KEY not in repr(joined)


async def test_a_non_originating_joiner_gets_the_shown_outcome_by_name(kernel):
    k = kernel
    _seed(k)
    own = await _send(k, _issue(id="keyread"), "alice")
    assert _real(own), own
    joined = await _send(
        k, {"target": "vault", "id": "keyread", "payload": {"type": "issue"}}, "eve"
    )
    assert _shown(joined), joined
    assert joined["pid"] == own["pid"], joined


async def test_the_original_caller_joining_again_gets_the_real_outcome(kernel):
    k = kernel
    _seed(k)
    own = await _send(k, _issue(id="mine"), "alice")
    again = await _send(
        k, {"target": "vault", "id": "mine", "payload": {"type": "issue"}}, "alice"
    )
    assert _real(again), again
    assert again["pid"] == own["pid"]


async def test_the_operator_is_an_identity_like_any_other(kernel):
    """The operator joining an AGENT's secret execution is not its caller (shown);
    an agent joining the OPERATOR's is not either (shown); the operator joining its
    own gets the real outcome."""
    k = kernel
    _seed(k)
    alices = await _send(k, _issue(id="a1"), "alice")
    op_join = await _send(
        k, {"target": "vault", "id": alices["pid"], "payload": {"type": "issue"}}
    )
    assert _shown(op_join), op_join
    ops = await _send(k, _issue(id="o1"))
    assert _real(ops), ops
    eve_join = await _send(
        k, {"target": "vault", "id": "o1", "payload": {"type": "issue"}}, "eve"
    )
    assert _shown(eve_join), eve_join
    op_again = await _send(k, {"target": "vault", "id": "o1", "payload": {"type": "x"}})
    assert _real(op_again), op_again


async def test_two_foreign_callers_through_one_leg_are_two_identities(kernel):
    """A bridge leg relays many callers of ONE peer kernel; each is its qualified
    `<guid>|<agent>`, not the leg."""
    k = kernel
    _seed(k)
    first = await _send(k, _issue(id="peer-read"), "leg", "P-g|a")
    assert _real(first), first
    other = await _send(
        k,
        {"target": "vault", "id": "peer-read", "payload": {"type": "issue"}},
        "leg",
        "P-g|b",
    )
    assert _shown(other), other
    same = await _send(
        k,
        {"target": "vault", "id": "peer-read", "payload": {"type": "issue"}},
        "leg",
        "P-g|a",
    )
    assert _real(same), same


async def test_a_joiner_waiting_on_an_execution_in_flight_gets_it_shown(kernel):
    global _release
    _release = asyncio.Event()
    k = kernel
    _seed(k)
    owner = asyncio.create_task(
        _send(
            k,
            {
                "target": "vault",
                "id": "slow",
                "payload": {"type": "slow_issue"},
                "secret": ["result.key"],
            },
            "alice",
        )
    )
    for _ in range(50):
        if k.ctx.resolve_name("slow") is not None:
            break
        await asyncio.sleep(0)
    joiner = asyncio.create_task(
        _send(k, {"target": "vault", "id": "slow", "payload": {"type": "x"}}, "eve")
    )
    await asyncio.sleep(0)
    _release.set()
    own, joined = await asyncio.gather(owner, joiner)
    assert _real(own), own
    assert _shown(joined), joined


async def test_a_join_of_an_unmarked_execution_is_unchanged(kernel):
    k = kernel
    _seed(k)
    own = await _send(
        k,
        {"target": "vault", "id": "plain", "payload": {"type": "echo", "text": "hi"}},
        "alice",
    )
    joined = await _send(
        k, {"target": "vault", "id": "plain", "payload": {"type": "echo"}}, "eve"
    )
    assert joined["result"] == own["result"] == {"echo": "hi"}, joined


async def test_carried_secret_resolves_an_item_id_as_send_does(kernel):
    """The question ai_core asks before a model's `<exec_id>` joins anything: an
    int is a message id, a string a caller's name; anything else, an id naming no
    held execution, and an unmarked execution are all False."""
    k = kernel
    _seed(k)
    marked = await _send(k, _issue(id="m1"), "alice")
    plain = await _send(
        k,
        {"target": "vault", "id": "p1", "payload": {"type": "echo", "text": "x"}},
        "alice",
    )
    assert k.ctx.carried_secret(marked["pid"]) is True
    assert k.ctx.carried_secret("m1") is True
    assert k.ctx.carried_secret(plain["pid"]) is False
    assert k.ctx.carried_secret("p1") is False
    assert k.ctx.carried_secret(str(marked["pid"])) is False, "a string is a name"
    for nothing in (True, None, 10_000, "nobody", ["m1"]):
        assert k.ctx.carried_secret(nothing) is False, nothing
