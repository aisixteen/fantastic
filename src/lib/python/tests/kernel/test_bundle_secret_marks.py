"""BUNDLE SECRET MARKS — a bundle names its own secret paths for a call's payload
(`payload_secret`) and for its reply (`reply_secret`). The kernel records them as
`[secret]`; the handler and the caller still get the real values."""

from __future__ import annotations

import types

from kernel import current_secret, register_unlisted

_seen: list[tuple[dict, tuple]] = []


async def _handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    if payload.get("type") == "reflect":
        return {"id": id}
    _seen.append((payload, current_secret()))
    if payload.get("type") == "get":
        return {"ref": "r", "fields": {"api_key": "sk-GET-REAL"}}
    return {"ok": True, "echo": payload.get("fields")}


def _payload_secret(payload):
    if payload.get("type") == "set":
        return ["fields.api_key"]
    return []


def _reply_secret(payload, reply):
    if payload.get("type") == "get":
        return ["fields.api_key"]
    return []


_mod = types.ModuleType("bundle_secret_probe.tools")
_mod.handler = _handler
_mod.payload_secret = _payload_secret
_mod.reply_secret = _reply_secret
_mod.VERBS = {}
register_unlisted("bundle_secret_probe.tools", _mod)


async def _mk(kernel) -> str:
    out = await kernel.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": kernel.id,
                    "handler_module": "bundle_secret_probe.tools",
                },
            }
        ]
    )
    return out[0]["result"]["id"]


async def test_a_bundle_names_its_own_secret_paths(seeded_kernel):
    aid = await _mk(seeded_kernel)
    out = (
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "set", "fields": {"api_key": "sk-SET-REAL"}}}]
        )
    )[0]
    assert out["status"] == "completed"
    # the handler got the real value and the bundle's marks
    payload, marks = _seen[-1]
    assert payload["fields"]["api_key"] == "sk-SET-REAL"
    assert marks == ("fields.api_key",)
    # the call row keeps it shown
    row = seeded_kernel.ctx.rows[out["pid"]]
    assert row["payload"]["fields"]["api_key"] == "[secret]"
    assert row["secret"] == ["fields.api_key"]
    # the ledger never holds the value
    assert "sk-SET-REAL" not in repr(seeded_kernel.ctx.rows)


async def test_get_reply_value_is_secret_in_settle_row_but_caller_gets_it(seeded_kernel):
    aid = await _mk(seeded_kernel)
    events: list[dict] = []
    off = seeded_kernel.ctx.add_state_subscriber(events.append)
    try:
        out = (
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "get", "ref": "r"}, "id": "get-1"}]
            )
        )[0]
    finally:
        off()
    # the caller gets the value whole
    assert out["result"]["fields"]["api_key"] == "sk-GET-REAL"
    # the settle row and the state stream hold only the mark
    settles = [r for r in seeded_kernel.ctx.rows.values() if r.get("kind") == "settle"]
    assert settles, "no settle row"
    assert "sk-GET-REAL" not in repr(settles)
    assert "[secret]" in repr(settles)
    assert "sk-GET-REAL" not in repr(events)


async def test_memory_store_rows_never_hold_the_value(seeded_kernel):
    aid = await _mk(seeded_kernel)
    await seeded_kernel.send(
        [{"target": aid, "payload": {"type": "set", "fields": {"api_key": "sk-STORE-REAL"}}}]
    )
    await seeded_kernel.send([{"target": aid, "payload": {"type": "get", "ref": "r"}}])
    for row in seeded_kernel.ctx.rows.values():
        assert "sk-STORE-REAL" not in repr(row)
        assert "sk-GET-REAL" not in repr(row)


async def test_a_bundle_without_secret_paths_is_unchanged(seeded_kernel):
    aid = await _mk(seeded_kernel)
    out = (
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "other", "fields": {"api_key": "plain"}}}]
        )
    )[0]
    assert out["status"] == "completed"
    row = seeded_kernel.ctx.rows[out["pid"]]
    assert row["payload"]["fields"]["api_key"] == "plain"
    assert "secret" not in row


async def test_a_missing_secret_path_is_ignored(seeded_kernel):
    aid = await _mk(seeded_kernel)
    out = (
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "set", "other": 1}}]
        )
    )[0]
    assert out["status"] == "completed"
    assert seeded_kernel.ctx.rows[out["pid"]]["payload"] == {"type": "set", "other": 1}


# ─── Rust twin (#170) ───────────────────────────────────────────────────────

_err_mod = types.ModuleType("secret_error_probe.tools")


async def _err_handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    return {"error": "bad key sk-ERR-REAL"}


_err_mod.handler = _err_handler
_err_mod.reply_secret = lambda payload, reply: (
    ["error"] if payload.get("type") == "fail" else []
)
_err_mod.VERBS = {}
register_unlisted("secret_error_probe.tools", _err_mod)


async def test_a_call_error_echoing_a_secret_is_shown_in_the_row(seeded_kernel):
    """A bundle's reply names its `error` as a secret path because the error echoes
    the value: every row shows `[secret]`, never the value; the caller still gets the
    error whole."""
    seeded_kernel.create("secret_error_probe.tools", id="ep")
    out = (
        await seeded_kernel.send(
            [{"target": "ep", "payload": {"type": "fail"}, "id": "f1"}]
        )
    )[0]
    assert "sk-ERR-REAL" in out["result"]["error"], "the caller gets the error whole"
    rows = repr(list(seeded_kernel.ctx.rows.values()))
    assert "sk-ERR-REAL" not in rows, f"a row kept the error text: {rows}"
    assert "[secret]" in rows
