"""The relay password lives in a credential_store — never on the record.

Audit IO-7 / F4 (2026-10-03): `relay_token` (the relay group password, sent as
`X-Fantastic-Auth`) was a plain record field — in `list_agents`, and in cleartext
in `.fantastic/agents/<id>/agent.yaml`. Owner rule: secrets live only in a
credential_store. The connector's record names WHERE the password lives —
`credentials: {store, ref}` — and before every dial it reads the ref's `password`
field AS ITSELF, the answer marked `secret: ['result']`, pinned to the relay URL's
host, exactly as an AI backend reads its key. `relay_token` is gone.
"""

from __future__ import annotations

import pytest

from _testkit import boot_root, persist
from _testkit_relay import FakeRelayHub
from relay_connector import _relay as rc_relay
from io_bridge import _engine

PW = "relay-group-pw-SECRET-9f9f"
PW2 = "relay-group-pw-ROTATED-0e0e"


async def _op(k, payload, **item) -> dict:
    return (await k.send([{"target": k.id, "payload": payload, **item}]))[0]["result"]


async def _set_pw(k, value: str) -> None:
    out = (
        await k.send(
            [
                {
                    "target": "creds",
                    "payload": {
                        "type": "set",
                        "ref": "relay/group",
                        "fields": {"password": value},
                    },
                    "secret": ["payload.fields"],
                }
            ]
        )
    )[0]
    assert out["result"].get("stored") is True, out


@pytest.fixture
def relay_kernel(tmp_path, monkeypatch):
    """A kernel with a credential store holding the relay password (ref
    `relay/group`, bound to the relay's host, readable by the leg `rl`), and the
    network dial stubbed over the fake hub — every dial's headers recorded."""
    monkeypatch.chdir(tmp_path)
    hub = FakeRelayHub()
    dials: list[dict] = []

    async def open_ws(url, headers):
        dials.append({"url": url, "headers": dict(headers)})
        return hub.register(url.rsplit("/", 1)[-1])

    monkeypatch.setattr(rc_relay, "_open_ws", open_ws)
    k = boot_root()
    made = k.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        refs={"relay/group": {"host": "relay:9443"}},
        callable_by={"rl": ["get"]},
    )
    assert made.get("id") == "creds", made
    yield k, dials
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _leg(k, **meta) -> dict:
    return await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "relay_connector.tools",
            "id": "rl",
            "transport": "relay",
            "relay_url": "ws://relay:9443",
            "guid": "A",
            "partner_guid": "B",
            "heartbeat": 0,
            "reconnect": 0,
            "can_call": {"creds": ["get"]},
            **meta,
        },
    )


async def test_the_password_is_read_from_the_store_and_never_recorded(
    relay_kernel, tmp_path
):
    k, dials = relay_kernel
    await _set_pw(k, PW)
    made = await _leg(k, credentials={"store": "creds", "ref": "relay/group"})
    assert made.get("id") == "rl", made
    booted = (await k.send([{"target": "rl", "payload": {"type": "boot"}}]))[0]
    assert booted["result"].get("booted") or booted["result"].get("already"), booted
    assert dials and dials[-1] == {
        "url": "ws://relay:9443/A",
        "headers": {"X-Fantastic-Auth": PW},
    }, dials
    # no record, listing, ledger or file of the tree carries it
    assert PW not in repr(k.get("rl"))
    listed = await _op(k, {"type": "list_agents"})
    assert PW not in repr(listed)
    ledger = await _op(k, {"type": "ledger"})
    assert PW not in repr(ledger)
    persist(k)
    on_disk = "".join(
        p.read_text() for p in (tmp_path / ".fantastic").rglob("*.yaml") if p.is_file()
    )
    assert PW not in on_disk


async def test_a_rotated_password_is_used_by_the_next_dial(relay_kernel):
    k, dials = relay_kernel
    await _set_pw(k, PW)
    await _leg(k, credentials={"store": "creds", "ref": "relay/group"})
    await k.send([{"target": "rl", "payload": {"type": "boot"}}])
    await _set_pw(k, PW2)
    r = (await k.send([{"target": "rl", "payload": {"type": "reconnect"}}]))[0]
    assert r["result"].get("booted") is True, r
    assert dials[-1]["headers"] == {"X-Fantastic-Auth": PW2}, dials


async def test_a_leg_with_no_credentials_fails_boot_loud(relay_kernel):
    k, dials = relay_kernel
    await _leg(k)
    out = (await k.send([{"target": "rl", "payload": {"type": "boot"}}]))[0]
    assert out["result"] == {
        "error": "bridge: relay boot failed: relay_connector: no `credentials` on "
        "rl — name the relay password's credential store and ref (update_agent rl "
        "credentials={store: <id>, ref: <ref>}; the ref's field `password`, bound "
        "to the relay's host)"
    }, out
    assert not dials, "nothing dialed"


async def test_a_store_refusal_fails_the_dial_naming_store_and_ref(relay_kernel):
    """Pinned to the relay URL's host: a ref bound elsewhere is refused by the
    store, and the boot names the store, the ref and the store's own words."""
    k, dials = relay_kernel
    await _set_pw(k, PW)
    await _leg(
        k,
        relay_url="ws://elsewhere:1",
        credentials={"store": "creds", "ref": "relay/group"},
    )
    out = (await k.send([{"target": "rl", "payload": {"type": "boot"}}]))[0]
    assert out["result"] == {
        "error": "bridge: relay boot failed: relay_connector: credentials "
        "creds/relay/group: credential_store: ref 'relay/group' is bound to host "
        "'relay:9443', not 'elsewhere:1' — refused, nothing read"
    }, out
    assert not dials and PW not in repr(out)


@pytest.mark.parametrize(
    ("bad", "why"),
    [
        ("creds", "credentials must be {store: <credential_store id>, ref: <ref>}"),
        (
            {"store": "creds"},
            "credentials must be {store: <credential_store id>, ref: <ref>}",
        ),
        ({"store": "", "ref": "r"}, "credentials.store must be a non-empty string"),
    ],
)
async def test_a_malformed_credentials_field_is_refused_at_the_write(
    relay_kernel, bad, why
):
    k, _ = relay_kernel
    out = await _leg(k, credentials=bad)
    assert out == {"error": why, "reason": "invalid_meta"}, out
