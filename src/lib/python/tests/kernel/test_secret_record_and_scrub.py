"""Two edges of SECRET marks (owner-approved 2026-10-03):

- S2 — a mark on a `create_agent`/`update_agent` payload field is refused: the
  kernel writes every such field into the agent's RECORD (its `agent_created` /
  `agent_updated` events, `agent.yaml`), where the mark cannot hold. A value an
  agent must keep is a `credential_store` ref.
- S7 — the scrub replaces a dict KEY, a number or a bool whose text equals a secret
  value: a reply keyed by the token it was given, or a PIN echoed as an int, used
  to land in every record. Equal, not contained: a key is structure."""

from __future__ import annotations

import types

from kernel import register_unlisted
from kernel._secret import PLACEHOLDER, scrub

KEY = "sk-record-0123456789abcdef"
PIN = 987654321

_got: list[dict] = []


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "scrub probe", "verbs": {}}
    _got.append(payload)
    if verb == "open_session":
        return {"sessions": {payload["token"]: {"open": True}}, "count": 1}
    if verb == "echo_pin":
        return {"pin": payload["pin"], "tries": 3}
    return {"error": f"unknown verb {verb!r}"}


_mod = types.ModuleType("scrub_probe.tools")
_mod.handler = _probe
_mod.VERBS = {}
register_unlisted("scrub_probe.tools", _mod)

WHY = "a record field cannot be secret — use a credential_store ref"


async def _mk(k, aid="vault") -> str:
    out = await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "scrub_probe.tools",
                    "id": aid,
                },
            }
        ]
    )
    return out[0]["result"]["id"]


async def _ledger(k) -> list[dict]:
    r = await k.send([{"target": "kernel", "payload": {"type": "ledger"}}])
    return r[0]["result"]["executions"]


# ─── S2: a record field is never secret ──────────────────────────────────────


async def test_a_mark_on_a_create_or_update_agent_field_is_refused(seeded_kernel):
    k = seeded_kernel
    a = await _mk(k)
    events: list[dict] = []
    k.ctx.add_state_subscriber(events.append)
    create = {
        "type": "create_agent",
        "parent_id": k.id,
        "handler_module": "scrub_probe.tools",
        "id": "leaky",
        "env": {"TOKEN": KEY},
    }
    for payload, secret, text in (
        (
            create,
            ["payload.env.TOKEN"],
            "path 'payload.env.TOKEN' names a field create_agent writes into the "
            f"agent record: {WHY}",
        ),
        (
            create,
            [["payload", "env"]],
            "path ['payload', 'env'] names a field create_agent writes into the "
            f"agent record: {WHY}",
        ),
        (
            create,
            ["payload.id"],
            "path 'payload.id' names a field create_agent writes into the agent "
            f"record: {WHY}",
        ),
        (
            {"type": "update_agent", "id": a, "note": KEY},
            ["payload.note"],
            "path 'payload.note' names a field update_agent writes into the agent "
            f"record: {WHY}",
        ),
    ):
        out = (
            await k.send([{"target": "kernel", "payload": payload, "secret": secret}])
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, (secret, out)
        assert out["result"] == {
            "error": f"send: secret {text}",
            "reason": "bad_arg",
            "arg": "secret",
        }, (secret, out["result"])
    assert "leaky" not in k.ctx.agents
    assert "note" not in k.ctx.agents[a].record
    assert KEY not in repr(events), "a refused item must not reach the stream"


async def test_payload_type_is_still_judged_first(seeded_kernel):
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "update_agent", "id": "x"},
                    "secret": ["payload.type"],
                }
            ]
        )
    )[0]
    assert out["result"]["error"] == (
        "send: secret cannot hide payload.type — the verb is how every row is read"
    )


async def test_a_result_mark_on_create_agent_is_unchanged(seeded_kernel):
    """Only PAYLOAD fields become the record; a result mark masks the call's
    records and nothing else, as on any verb."""
    k = seeded_kernel
    out = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "scrub_probe.tools",
                        "id": "plain",
                    },
                    "secret": ["result"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed" and out["result"]["id"] == "plain", out


# ─── S7: keys, numbers and bools ─────────────────────────────────────────────


def test_a_short_secret_never_mangles_a_payload_key():
    """A one-letter secret inside `type` scrubs the verb's TEXT (as before) but
    never the key: every row is read by `payload.type`."""
    from kernel._secret import check, shown_payload

    payload = {"type": "set_key", "key": "y"}
    s, _ = check({"secret": ["payload.key"]}, payload)
    assert shown_payload(payload, s) == {"type": "set_ke<secret>", "key": PLACEHOLDER}


def test_scrub_replaces_a_key_a_number_and_a_bool_whose_text_is_a_secret():
    got = scrub(
        {
            "a": "xxabcxx",
            "k": ["abc"],
            "abc": "ab",
            "xabcx": 1,
            "n": 123,
            "m": 1234,
            "f": 1.5,
            "t": True,
            "z": None,
        },
        ["ab", "abc", "123", "1.5", "True"],
    )
    assert got == {
        "a": "xx<secret>xx",
        "k": ["<secret>"],
        "<secret>": "<secret>",
        # a key that only CONTAINS a secret is structure, kept
        "xabcx": 1,
        "n": "<secret>",
        "m": 1234,
        "f": "<secret>",
        # a bool's text is `True`/`False` (S7); null holds none
        "t": "<secret>",
        "z": None,
    }


async def test_a_reply_keyed_by_the_secret_is_recorded_scrubbed(seeded_kernel):
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "open_session", "token": KEY},
                    "secret": ["payload.token"],
                    "id": "sess-1",
                }
            ]
        )
    )[0]
    # the caller holds the real reply
    assert out["result"] == {"sessions": {KEY: {"open": True}}, "count": 1}
    row = next(e for e in await _ledger(k) if e["id"] == out["pid"])
    assert row["outcome"]["result"] == {
        "sessions": {PLACEHOLDER: {"open": True}},
        "count": 1,
    }, row
    assert KEY not in repr(await _ledger(k))


async def test_a_secret_number_echoed_as_a_number_is_recorded_scrubbed(
    seeded_kernel,
):
    k = seeded_kernel
    a = await _mk(k)
    out = (
        await k.send(
            [
                {
                    "target": a,
                    "payload": {"type": "echo_pin", "pin": PIN},
                    "secret": ["payload.pin"],
                    "id": "pin-1",
                }
            ]
        )
    )[0]
    assert out["result"] == {"pin": PIN, "tries": 3}
    row = next(e for e in await _ledger(k) if e["id"] == out["pid"])
    assert row["outcome"]["result"] == {"pin": PLACEHOLDER, "tries": 3}, row
    assert str(PIN) not in repr(await _ledger(k))
