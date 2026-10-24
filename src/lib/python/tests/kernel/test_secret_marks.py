"""SECRET MARKS — an item's `secret` paths are shown as `[secret]` in everything the
kernel keeps or shows of the call (call row, state stream, watcher mirrors); the
handler gets the real payload, and reads the marks as `current_secret()`."""

from __future__ import annotations

import types

from kernel import current_secret, register_unlisted

_seen: list[tuple[dict, tuple]] = []


async def _handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown", "reflect"):
        return {"id": id} if payload.get("type") == "reflect" else None
    _seen.append((payload, current_secret()))
    return {"ok": True}


_mod = types.ModuleType("secret_probe_agent.tools")
_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("secret_probe_agent.tools", _mod)


async def _mk(kernel) -> str:
    out = await kernel.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": kernel.id,
                    "handler_module": "secret_probe_agent.tools",
                },
            }
        ]
    )
    return out[0]["result"]["id"]


async def test_marked_value_is_shown_never_kept(seeded_kernel):
    aid = await _mk(seeded_kernel)
    events: list[dict] = []
    off = seeded_kernel.ctx.add_state_subscriber(events.append)
    try:
        out = (
            await seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {
                            "type": "set",
                            "ref": "r",
                            "fields": {"api_key": "sk-REAL"},
                        },
                        "secret": ["fields.api_key"],
                    }
                ]
            )
        )[0]
    finally:
        off()
    assert out["status"] == "completed"
    # the handler got the real value, and the marks
    payload, marks = _seen[-1]
    assert payload["fields"]["api_key"] == "sk-REAL" and marks == ("fields.api_key",)
    # the call row keeps it shown, with the marks
    row = seeded_kernel.ctx.rows[out["pid"]]
    assert row["payload"]["fields"]["api_key"] == "[secret]"
    assert row["payload"]["ref"] == "r" and row["secret"] == ["fields.api_key"]
    # nothing on the state stream carries it
    assert "sk-REAL" not in repr(events)
    assert any(
        (e.get("payload") or {}).get("fields", {}).get("api_key") == "[secret]"
        for e in events
    )


async def test_malformed_marks_are_refused(seeded_kernel):
    aid = await _mk(seeded_kernel)
    for bad in ("fields.api_key", [""], ["a..b"], [1], ["type"]):
        out = (
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "set"}, "secret": bad}]
            )
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, bad
        assert out["result"]["arg"] == "secret", bad


async def test_a_path_naming_nothing_is_skipped(seeded_kernel):
    aid = await _mk(seeded_kernel)
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "set", "x": 1},
                    "secret": ["nope.deep"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed"
    assert seeded_kernel.ctx.rows[out["pid"]]["payload"] == {"type": "set", "x": 1}


# ─── reply marks: a handler notes the secret paths of the reply it relayed ───


async def _noting_handler(id, payload, kernel):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    if payload.get("type") == "reflect":
        return {"id": id}
    from kernel import note_reply_secret  # noqa: PLC0415

    note_reply_secret(["fields.k", "type", "a..b"])
    return {"fields": {"k": "v"}}


_note_mod = types.ModuleType("secret_note_agent.tools")
_note_mod.handler = _noting_handler
_note_mod.VERBS = {}
register_unlisted("secret_note_agent.tools", _note_mod)


async def test_a_handler_noting_reply_secret_marks_its_settle_row(seeded_kernel):
    out_id = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "secret_note_agent.tools",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    out = (
        await seeded_kernel.send(
            [{"target": out_id, "payload": {"type": "get", "ref": "r"}, "id": "note-1"}]
        )
    )[0]
    # the caller gets the reply whole
    assert out["result"] == {"fields": {"k": "v"}}, out
    # only the well-formed, non-`type` path is a reply mark
    assert out["secret"] == ["fields.k"], out
    # the settle row (named: it keeps the result) shows the marked value as [secret]
    settles = [
        r
        for r in seeded_kernel.ctx.rows.values()
        if r.get("kind") == "settle" and r.get("parent") == out["pid"]
    ]
    assert settles, "no settle row"
    kept = settles[0]["outcome"]["result"]
    assert kept == {"fields": {"k": "[secret]"}}, kept
    # an unmarked call's outcome carries no `secret` key
    probe = await _mk(seeded_kernel)
    plain = (await seeded_kernel.send([{"target": probe, "payload": {"type": "x"}}]))[0]
    assert plain["result"] == {"ok": True} and "secret" not in plain, plain
