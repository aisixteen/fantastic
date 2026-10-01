"""SECRET VALUES on a call (owner, 2026-10-01; `kernel/_secret.py`): the caller
marks paths on the batch item; the target gets the real payload and the caller the
real reply, while every record and mirror of the call shows `<secret>`."""

from __future__ import annotations

import sys
import types

from kernel._secret import PLACEHOLDER

KEY = "sk-live-0123456789abcdef"
ISSUED = "tok-issued-fedcba9876543210"

_got: list[dict] = []


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "secret probe", "verbs": {}}
    _got.append(payload)
    if verb == "set_key":
        return {"ok": True, "len": len(payload["key"])}
    if verb == "issue":
        return {"key": ISSUED, "note": "issued"}
    if verb == "fail":
        raise RuntimeError(f"rejected key {payload['key']}")
    return {"error": f"unknown verb {verb!r}"}


_mod = types.ModuleType("secret_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
sys.modules["secret_probe.tools"] = _mod


async def _mk(k, aid="vault") -> str:
    out = await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "secret_probe.tools",
                    "id": aid,
                },
            }
        ]
    )
    return out[0]["result"]["id"]


def _call_row(k, pid):
    return k.ctx.rows.get(pid)


async def _ledger(k) -> list[dict]:
    r = await k.send([{"target": "kernel", "payload": {"type": "ledger"}}])
    return r[0]["result"]["executions"]


def _mentions(x, value: str) -> bool:
    return value in repr(x)


# ─── the item field ──────────────────────────────────────────────────────────


async def test_a_bad_secret_field_is_refused_before_anything_runs(seeded_kernel):
    k = seeded_kernel
    a = await _mk(k)
    _got.clear()
    for secret, why in (
        ("payload.key", "non-empty list"),
        ([], "non-empty list"),
        (["key"], "must start with 'payload.'"),
        (["payload"], "must start with 'payload.'"),
        (["payload.type"], "cannot hide payload.type"),
        (["payload.nope"], "names nothing in the payload"),
        (["payload..key"], "empty step"),
    ):
        out = (
            await k.send(
                [
                    {
                        "target": a,
                        "payload": {"type": "set_key", "key": KEY},
                        "secret": secret,
                    }
                ]
            )
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, (secret, out)
        r = out["result"]
        assert r["reason"] == "bad_arg" and r["arg"] == "secret", (secret, r)
        assert why in r["error"], (secret, r)
    assert _got == [], "a refused item must not reach the handler"


# ─── a secret payload field ──────────────────────────────────────────────────


async def test_the_target_gets_the_value_and_every_record_shows_the_placeholder(
    seeded_kernel,
):
    k = seeded_kernel
    a = await _mk(k)
    events: list[dict] = []
    k.ctx.add_state_subscriber(events.append)
    _got.clear()
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "set_key", "ref": "openai", "key": KEY},
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    # the handler and the caller see the real thing
    assert _got == [{"type": "set_key", "ref": "openai", "key": KEY}]
    assert out["status"] == "completed" and out["result"] == {
        "ok": True,
        "len": len(KEY),
    }
    # the call row: placeholder, the other fields intact, the PATHS recorded
    row = _call_row(k, out["pid"])
    assert row["payload"] == {"type": "set_key", "ref": "openai", "key": PLACEHOLDER}
    assert row["secret"] == ["payload.key"]
    # the state stream's send event — payload AND summary
    sends = [e for e in events if e.get("kind") == "send" and e.get("agent_id") == a]
    assert sends and not any(_mentions(e, KEY) for e in sends), sends
    assert sends[0]["payload"]["key"] == PLACEHOLDER
    # the ledger read-back
    assert not _mentions(await _ledger(k), KEY)


async def test_a_watcher_mirror_shows_the_placeholder(seeded_kernel):
    k = seeded_kernel
    a = await _mk(k)
    k.ctx.ensure_inbox("spy")
    k.watch(a, "spy")
    await k.send(
        [
            {
                "target": a,
                "payload": {"type": "set_key", "key": KEY},
                "secret": ["payload.key"],
            }
        ]
    )
    q = k.ctx.inboxes["spy"]
    mirrored = [q.get_nowait() for _ in range(q.qsize())]
    assert mirrored and not any(_mentions(m, KEY) for m in mirrored), mirrored


async def test_an_error_text_quoting_the_value_is_scrubbed_in_the_record(
    seeded_kernel,
):
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "fail", "key": KEY},
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    assert out["status"] == "failed"
    assert KEY in out["result"]["error"], "the caller holds the real outcome"
    led = await _ledger(k)
    assert not _mentions(led, KEY), led
    mine = next(e for e in led if e["id"] == out["pid"])
    assert PLACEHOLDER in mine["outcome"]["error"], mine


# ─── a secret result ─────────────────────────────────────────────────────────


async def test_a_secret_result_reaches_the_caller_and_is_kept_as_the_placeholder(
    seeded_kernel,
):
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "issue"},
                    "id": "issue-1",  # named: the settle keeps the whole result
                    "secret": ["result.key"],
                }
            ]
        )
    )[0]
    assert out["result"] == {"key": ISSUED, "note": "issued"}
    led = await _ledger(k)
    assert not _mentions(led, ISSUED), led
    mine = next(e for e in led if e.get("name") == "issue-1")
    assert mine["outcome"]["result"] == {"key": PLACEHOLDER, "note": "issued"}


async def test_the_whole_result_can_be_secret(seeded_kernel):
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "issue"},
                    "id": "issue-2",
                    "secret": ["result"],
                }
            ]
        )
    )[0]
    assert out["result"]["key"] == ISSUED
    mine = next(e for e in await _ledger(k) if e.get("name") == "issue-2")
    assert mine["outcome"]["result"] == PLACEHOLDER


# ─── the record cannot answer a replay ───────────────────────────────────────


async def _boot_with_store(tmp_path, monkeypatch):
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    k = boot_root()
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "ram_store.tools",
                    "id": "memory",
                    "operator": True,
                    "admins": ["kernel"],
                },
            }
        ]
    )
    k.ctx.update(k.id, memory_store_id="memory")
    out = await k.ctx.start(boot_all=False)
    assert out.get("started"), out
    return k


async def test_the_store_never_holds_the_value_and_a_replay_is_refused(
    tmp_path, monkeypatch
):
    k = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k)
    _got.clear()
    first = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "set_key", "key": KEY},
                    "id": "key-1",
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    assert first["status"] == "completed"
    # in RAM the execution still answers a re-send (the same process holds it)
    again = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "set_key", "key": KEY},
                    "id": "key-1",
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    assert again["result"] == first["result"] and len(_got) == 1
    await k.ctx.flush_executions()
    tables = (await k.send([{"target": "memory", "payload": {"type": "dump"}}]))[0][
        "result"
    ]["tables"]
    assert not _mentions(tables, KEY), "the memory store holds the secret value"
    # once it lives only in the store, the record cannot answer — refused, loud
    k.ctx.executions.clear()
    k.ctx.names.clear()
    replay = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "set_key", "key": KEY},
                    "id": "key-1",
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    assert replay["status"] == "failed", replay
    assert (
        replay["result"]["reason"] == "bad_arg"
        and "carried secrets" in replay["result"]["error"]
    ), replay
    assert len(_got) == 1, "the refused replay must not run the handler"


async def test_a_short_secret_never_mangles_the_kernels_own_words(seeded_kernel):
    """Only the RESULT is scrubbed: a one-letter secret must not rewrite the
    recorded `status` ("completed" contains an "e")."""
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "set_key", "key": "e"},
                    "id": "short-1",
                    "secret": ["payload.key"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed"
    mine = next(e for e in await _ledger(k) if e.get("name") == "short-1")
    assert mine["status"] == "completed" and mine["outcome"]["status"] == "completed"
