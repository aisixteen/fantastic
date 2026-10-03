"""SECRET VALUES on a call (owner, 2026-10-01; `kernel/_secret.py`): the caller
marks paths on the batch item; the target gets the real payload and the caller the
real reply, while every record and mirror of the call shows `<secret>`."""

from __future__ import annotations

import types

from kernel import current_secret, register_unlisted
from kernel._secret import PLACEHOLDER

KEY = "sk-live-0123456789abcdef"
KEY2 = "sk-live-dotted-key-5a5a5a5a"
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
    if verb == "hold":
        return {"ok": True}
    if verb == "marks":
        return {"marks": list(current_secret())}
    if verb == "issue_dotted":
        return {"a.b": ISSUED, "a": {"b": "nested-plain"}, "items": ["x", ISSUED]}
    return {"error": f"unknown verb {verb!r}"}


_mod = types.ModuleType("secret_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("secret_probe.tools", _mod)


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


# ─── a mark as an ARRAY OF STEPS (owner, 2026-10-03) ─────────────────────────


async def test_an_array_mark_masks_a_literal_dotted_key_and_a_dotted_one_does_not(
    seeded_kernel,
):
    """`["payload", "a.b"]` is the key `"a.b"`, taken literally; the dotted string
    `payload.a.b` keeps meaning `a` → `b`."""
    k = seeded_kernel
    a = await _mk(k)
    events: list[dict] = []
    k.ctx.add_state_subscriber(events.append)
    _got.clear()
    sent = {"type": "hold", "a.b": KEY, "a": {"b": "nested-plain"}}
    out = (
        await k.send([{"target": a, "payload": sent, "secret": [["payload", "a.b"]]}])
    )[0]
    assert out["status"] == "completed" and out["result"] == {"ok": True}, out
    assert _got == [sent], "the target gets the real payload"
    row = _call_row(k, out["pid"])
    assert row["payload"] == {
        "type": "hold",
        "a.b": PLACEHOLDER,
        "a": {"b": "nested-plain"},
    }, row
    assert row["secret"] == [["payload", "a.b"]], row
    sends = [e for e in events if e.get("kind") == "send" and e.get("agent_id") == a]
    assert sends and not any(_mentions(e, KEY) for e in sends), sends
    assert not _mentions(await _ledger(k), KEY)
    # the dotted spelling of the same text is the NESTED path
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {
                        "type": "hold",
                        "a.b": "literal-plain",
                        "a": {"b": KEY},
                    },
                    "secret": ["payload.a.b"],
                }
            ]
        )
    )[0]
    assert _call_row(k, out["pid"])["payload"] == {
        "type": "hold",
        "a.b": "literal-plain",
        "a": {"b": PLACEHOLDER},
    }


async def test_a_mixed_list_of_string_and_array_marks(seeded_kernel):
    """Both spellings in one list: every payload and result mark masks, the call
    row records the marks as given, and the handler reads them back as given."""
    k = seeded_kernel
    a = await _mk(k)
    marks = [
        "payload.key",
        ["payload", "a.b"],
        ["result", "a.b"],
        ["result", "items", 1],
    ]
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "issue_dotted", "key": KEY, "a.b": KEY2},
                    "id": "mixed-1",  # named: the settle keeps the whole result
                    "secret": marks,
                }
            ]
        )
    )[0]
    assert out["result"] == {
        "a.b": ISSUED,
        "a": {"b": "nested-plain"},
        "items": ["x", ISSUED],
    }, "the caller holds the real reply"
    row = _call_row(k, out["pid"])
    assert row["secret"] == marks, row
    assert row["payload"] == {
        "type": "issue_dotted",
        "key": PLACEHOLDER,
        "a.b": PLACEHOLDER,
    }, row
    led = await _ledger(k)
    assert not any(_mentions(led, v) for v in (KEY, KEY2, ISSUED)), led
    mine = next(e for e in led if e.get("name") == "mixed-1")
    assert mine["outcome"]["result"] == {
        "a.b": PLACEHOLDER,
        "a": {"b": "nested-plain"},
        "items": ["x", PLACEHOLDER],
    }, mine
    # `current_secret()` is the marks exactly as given, an array as a list
    got = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "marks", "key": KEY, "a.b": KEY2},
                    "secret": marks,
                }
            ]
        )
    )[0]
    assert got["result"] == {"marks": marks}, got


async def test_array_steps_are_exact_an_int_indexes_a_list_a_string_names_a_key(
    seeded_kernel,
):
    k = seeded_kernel
    a = await _mk(k)
    payload = {"type": "hold", "items": [KEY, "plain"], "m": {"0": KEY2}}
    for secret, shown in (
        ([["payload", "items", 0]], {**payload, "items": [PLACEHOLDER, "plain"]}),
        ([["payload", "m", "0"]], {**payload, "m": {"0": PLACEHOLDER}}),
        (["payload.items.0"], {**payload, "items": [PLACEHOLDER, "plain"]}),
    ):
        out = (await k.send([{"target": a, "payload": payload, "secret": secret}]))[0]
        assert out["status"] == "completed", (secret, out)
        assert _call_row(k, out["pid"])["payload"] == shown, secret
    # a string step is never an index, an int step never a key
    for secret in ([["payload", "items", "0"]], [["payload", "m", 0]]):
        out = (await k.send([{"target": a, "payload": payload, "secret": secret}]))[0]
        assert out["result"]["error"] == (
            f"send: secret path {secret[0]!r} names nothing in the payload"
        ), out


async def test_each_malformed_mark_is_refused_with_its_own_text(seeded_kernel):
    """The array refusals, and the dotted-string ones byte for byte unchanged."""
    k = seeded_kernel
    a = await _mk(k)
    _got.clear()
    step = (
        "is neither a string (an object key) nor a non-negative integer (a list index)"
    )
    root = "must start with 'payload' and a step under it, or with 'result'"
    for secret, text in (
        ([[]], "path [] has no steps"),
        ([["payload", True]], f"path ['payload', True] step True {step}"),
        ([["payload", 1.5]], f"path ['payload', 1.5] step 1.5 {step}"),
        ([["payload", None]], f"path ['payload', None] step None {step}"),
        ([["payload", -1]], f"path ['payload', -1] step -1 {step}"),
        ([["payload", ["a"]]], f"path ['payload', ['a']] step ['a'] {step}"),
        ([["key"]], f"path ['key'] {root}"),
        ([[0, "key"]], f"path [0, 'key'] {root}"),
        ([["payload"]], f"path ['payload'] {root}"),
        (
            [["payload", "type"]],
            "cannot hide payload.type — the verb is how every row is read",
        ),
        (
            [["payload", "nope"]],
            "path ['payload', 'nope'] names nothing in the payload",
        ),
        (
            [["payload", "a", "b"]],
            "path ['payload', 'a', 'b'] names nothing in the payload",
        ),
        (
            ["payload.key", ["payload", "nope"]],
            "path ['payload', 'nope'] names nothing in the payload",
        ),
        ([5], "path must be a dotted string or an array of steps, got 5"),
        (
            [{"payload": "key"}],
            "path must be a dotted string or an array of steps, got {'payload': 'key'}",
        ),
        # the dotted-string texts, unchanged
        ("payload.key", "must be a non-empty list of paths, got 'payload.key'"),
        ([], "must be a non-empty list of paths, got []"),
        ([""], "path must be a non-empty string, got ''"),
        (["payload..key"], "path 'payload..key' has an empty step"),
        (["key"], "path 'key' must start with 'payload.' or be 'result[.…]'"),
        (["payload"], "path 'payload' must start with 'payload.' or be 'result[.…]'"),
        (
            ["payload.type"],
            "cannot hide payload.type — the verb is how every row is read",
        ),
        (["payload.a.b"], "path 'payload.a.b' names nothing in the payload"),
    ):
        out = (
            await k.send(
                [
                    {
                        "target": a,
                        "payload": {"type": "set_key", "key": KEY, "a.b": KEY2},
                        "secret": secret,
                    }
                ]
            )
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, (secret, out)
        assert out["result"] == {
            "error": f"send: secret {text}",
            "reason": "bad_arg",
            "arg": "secret",
        }, (secret, out["result"])
    assert _got == [], "a refused item must not reach the handler"
