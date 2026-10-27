"""The ONE refusal shape and the agent-id guard, pinned.

`kernel/_refusal.py` is where candidate K would add codes (ai_core hand-builds
dicts with reasons outside `CODES`, e.g. `memory_store`). These hold the builder's
contract — the closed set, the key order, the helper texts — and every
`guard_agent_id` refusal as `create_agent` answers it.
"""

from __future__ import annotations

import pytest

from kernel._refusal import CODES, bad, missing, refusal


def test_an_unknown_code_is_a_programming_error():
    assert CODES == {
        "unauthorized",
        "unknown_agent",
        "missing_arg",
        "bad_arg",
        "invalid_meta",
        "locked",
        "not_here",
        "exists",
        "unhandled",
        "not_found",
        "draining",
        "no_consumer",
        "not_connected",
        "transport_error",
        "transport_closed",
        "timeout",
        "secret_found",
        "failed",
        "unbound_ports",
        "bad_binding",
        "code_not_allowed",
        "limit",
        "not_allowed",
        "grant_failed",
        "unknown_tier",
        "bad_tier",
        "no_root",
        "not_spawned",
        "disabled",
        "store_refused",
        "already_held",
        "not_held",
        "not_in_subtree",
    }
    with pytest.raises(ValueError) as e:
        refusal("memory_store", "x")
    assert (
        str(e.value) == f"unknown refusal code 'memory_store'; one of {sorted(CODES)}"
    )


def test_the_import_codes_build_and_carry_their_reason():
    for code in (
        "secret_found",
        "failed",
        "unbound_ports",
        "bad_binding",
        "code_not_allowed",
    ):
        assert refusal(code, "e") == {"error": "e", "reason": code}
# twin: every_refusal_carries_both_fields
# twin: missing_and_bad_name_the_argument
# twin: extra_facts_ride_at_the_top_level
# twin: every_builder_serializes_in_pythons_key_order
# twin: missing_hint_orders_hint_before_arg


def test_key_order_and_the_optional_fields():
    r = refusal("bad_arg", "e", hint="h", see="s", arg="a", extra=1)
    assert list(r) == ["error", "reason", "hint", "see", "arg", "extra"]
    assert refusal("bad_arg", "e") == {"error": "e", "reason": "bad_arg"}
    assert missing("v", "x", hint="h") == {
        "error": "v: x required",
        "reason": "missing_arg",
        "hint": "h",
        "arg": "x",
    }
    assert bad("v", "x", "an int") == {
        "error": "v: x must be an int",
        "reason": "bad_arg",
        "arg": "x",
    }


@pytest.mark.parametrize(
    "aid, want",
    [
        ("", "a non-empty string"),
        ("a/b", "a single path segment (no separators); got 'a/b'"),
        ("a\\b", "a single path segment (no separators); got 'a\\\\b'"),
        ("..", "a name that is not a path traversal; got '..'"),
        ("x..y", "a name that is not a path traversal; got 'x..y'"),
        (".hidden", "a name that is not hidden; got '.hidden'"),
        ("123", "a name, not a number (message ids are ints); got '123'"),
        (
            "g1|scout",
            "a local name without `|` (it marks a foreign `guid|agent`); got 'g1|scout'",
        ),
        ("kernel", "a name other than the reserved 'kernel'"),
    ],
)
async def test_guard_refuses_all_digit_ids_and_accepts_colons(seeded_kernel, aid, want):
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "id": aid,
                    },
                }
            ]
        )
    )[0]["result"]
    assert r == {
        "error": f"create_agent: id must be {want}",
        "reason": "bad_arg",
        "arg": "id",
    }, r


async def test_the_id_index_is_bounded(seeded_kernel):
    """The count is honest even though the list is clipped: one typo must not turn
    into a context dump."""
    from kernel import Kernel

    assert Kernel.UNKNOWN_AGENT_IDS_MAX == 40
    for i in range(200):
        seeded_kernel.create("file_bridge.tools", id=f"agent_{i:03}", allow=["."])
    r = (
        await seeded_kernel.send([{"target": "nope", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["reason"] == "unknown_agent", r
    assert len(r["agents"]) == Kernel.UNKNOWN_AGENT_IDS_MAX, r
    assert r["agent_count"] == len(seeded_kernel.ctx.agents) > 200, r


def test_every_refusal_code_is_in_the_closed_set():
    for code in (
        "limit",
        "not_allowed",
        "grant_failed",
        "unknown_tier",
        "bad_tier",
        "no_root",
        "not_spawned",
        "disabled",
        "store_refused",
        "already_held",
        "not_held",
        "not_in_subtree",
    ):
        assert code in CODES
        assert refusal(code, "x") == {"error": "x", "reason": code}



def test_new_bundle_codes_are_in_closed_set():
    from kernel import _refusal

    for name in (
        "LIMIT",
        "NOT_ALLOWED",
        "GRANT_FAILED",
        "UNKNOWN_TIER",
        "BAD_TIER",
        "NO_ROOT",
        "NOT_SPAWNED",
        "DISABLED",
        "STORE_REFUSED",
        "ALREADY_HELD",
        "NOT_HELD",
        "NOT_IN_SUBTREE",
    ):
        code = getattr(_refusal, name)
        assert code == name.lower() and code in CODES
        assert refusal(code, "x") == {"error": "x", "reason": code}
