"""credential_store: secrets by ref, bound to a host, read only with a secret mark,
behind per-ref grants (owner, 2026-10-01). Tests never touch the real keychain or
home: the file backend writes under tmp_path, the keychain backend runs on an
in-memory keyring."""

from __future__ import annotations

import os
import stat

import keyring
import pytest
from keyring.backend import KeyringBackend

from kernel import sender_context

KEY = "sk-live-0123456789abcdef"
KEY2 = "sk-live-rotated-fedcba98"
MARK_SET = ["payload.fields"]
MARK_GET = ["result"]


async def _call(k, payload, *, as_=None, secret=None, target="creds"):
    item = {"target": target, "payload": payload}
    if secret is not None:
        item["secret"] = secret
    if as_ is None:
        return (await k.send([item]))[0]
    with sender_context(as_):
        return (await k.send([item]))[0]


def _mk(k, tmp_path, **meta):
    k.ctx.update(k.id, guid="g1")
    for aid in ("alice", "bob", "eve", "maker"):
        k.create(None, id=aid, can_call={"creds": "*"})
    rec = k.create(
        "credential_store.tools",
        id="creds",
        callable_by={"alice": "*", "bob": "*", "eve": "*", "maker": "*"},
        **{"file": str(tmp_path / "creds.yaml"), **meta},
    )
    assert rec.get("id") == "creds", rec
    return k


async def _seed(k):
    out = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "https://api.openai.com/v1",
            "fields": {"api_key": KEY},
        },
        secret=MARK_SET,
    )
    assert out["status"] == "completed", out
    k.ctx.update(
        "creds",
        refs={
            **k.ctx.agents["creds"].record["refs"],
            "openai/main": {
                **k.ctx.agents["creds"].record["refs"]["openai/main"],
                "readers": ["alice"],
                "writers": ["bob"],
            },
        },
    )
    return out


# ─── the file backend ────────────────────────────────────────────────────────


async def test_set_stores_the_value_0600_and_the_record_keeps_only_metadata(
    seeded_kernel, tmp_path
):
    k = _mk(seeded_kernel, tmp_path)
    out = await _seed(k)
    assert out["result"] == {
        "ref": "openai/main",
        "host": "api.openai.com",
        "fields": ["api_key"],
        "stored": True,
    }
    f = tmp_path / "creds.yaml"
    assert stat.S_IMODE(f.stat().st_mode) == 0o600
    assert KEY in f.read_text()
    rec = k.ctx.agents["creds"].record
    assert KEY not in repr(rec)
    assert rec["refs"]["openai/main"]["host"] == "api.openai.com"
    assert rec["refs"]["openai/main"]["fields"] == ["api_key"]


async def test_values_move_only_with_a_secret_mark(seeded_kernel, tmp_path):
    k = _mk(seeded_kernel, tmp_path)
    unmarked = await _call(
        k, {"type": "set", "ref": "r", "host": "h", "fields": {"api_key": KEY}}
    )
    assert unmarked["result"]["reason"] == "bad_arg"
    assert unmarked["result"]["arg"] == "secret"
    assert not (tmp_path / "creds.yaml").exists(), "an unmarked set stores nothing"
    await _seed(k)
    unmarked_get = await _call(k, {"type": "get", "ref": "openai/main"})
    assert unmarked_get["result"]["arg"] == "secret", unmarked_get
    got = await _call(k, {"type": "get", "ref": "openai/main"}, secret=MARK_GET)
    assert got["result"] == {
        "ref": "openai/main",
        "host": "api.openai.com",
        "fields": {"api_key": KEY},
    }
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger), "the ledger must never hold the value"


async def test_readers_read_writers_rotate_strangers_see_nothing(
    seeded_kernel, tmp_path
):
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    # a reader reads
    a = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert a["result"]["fields"] == {"api_key": KEY}, a
    # a reader may not write — told so, since it knows the ref
    w = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": {"api_key": KEY2},
        },
        as_="alice",
        secret=MARK_SET,
    )
    assert w["result"]["reason"] == "unauthorized", w
    # a writer rotates; the reader's next read gets the new value
    r = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": {"api_key": KEY2},
        },
        as_="bob",
        secret=MARK_SET,
    )
    assert r["result"]["stored"] is True, r
    a2 = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert a2["result"]["fields"] == {"api_key": KEY2}
    # a stranger is answered as if the ref did not exist
    e = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="eve", secret=MARK_GET
    )
    assert (
        e["result"]["reason"] == "not_found" and "no such ref" in e["result"]["error"]
    )
    assert (await _call(k, {"type": "list"}, as_="eve"))["result"] == {"refs": []}
    # list is metadata only
    listed = (await _call(k, {"type": "list"}, as_="alice"))["result"]["refs"]
    assert listed == [
        {
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": ["api_key"],
            "read": True,
            "write": False,
        }
    ]
    assert KEY2 not in repr(listed)


async def test_a_creator_owns_the_ref_it_makes_others_cannot_make_one(
    seeded_kernel, tmp_path
):
    k = _mk(seeded_kernel, tmp_path, creators=["maker"])
    made = await _call(
        k,
        {"type": "set", "ref": "mine", "host": "x.example", "fields": {"token": KEY}},
        as_="maker",
        secret=MARK_SET,
    )
    assert made["result"]["stored"] is True, made
    entry = k.ctx.agents["creds"].record["refs"]["mine"]
    assert entry["readers"] == ["maker"] and entry["writers"] == ["maker"]
    nope = await _call(
        k,
        {"type": "set", "ref": "theirs", "host": "x.example", "fields": {"token": KEY}},
        as_="eve",
        secret=MARK_SET,
    )
    assert nope["result"]["reason"] == "not_found", nope


async def test_a_subtree_grant_covers_descendants(seeded_kernel, tmp_path):
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    k.ctx.agents["alice"].create(None, id="child", can_call={"creds": "*"})
    cb = dict(k.ctx.agents["creds"].record["callable_by"])
    cb["child"] = "*"
    refs = k.ctx.agents["creds"].record["refs"]
    refs["openai/main"]["readers"] = ["$subtree:alice"]
    k.ctx.update("creds", refs=refs, callable_by=cb)
    got = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="child", secret=MARK_GET
    )
    assert got["result"]["fields"] == {"api_key": KEY}, got


async def test_forget_drops_the_value_and_the_grants(seeded_kernel, tmp_path):
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    out = await _call(k, {"type": "forget", "ref": "openai/main"}, as_="bob")
    assert out["result"] == {"forgotten": True, "ref": "openai/main"}
    assert "openai/main" not in k.ctx.agents["creds"].record["refs"]
    assert KEY not in (tmp_path / "creds.yaml").read_text()


async def test_a_file_others_can_read_is_refused_loud(seeded_kernel, tmp_path):
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    os.chmod(tmp_path / "creds.yaml", 0o644)
    out = await _call(k, {"type": "get", "ref": "openai/main"}, secret=MARK_GET)
    assert out["status"] == "failed", out
    assert "mode 0600" in out["result"]["error"] and "0o644" in out["result"]["error"]


# ─── the keychain backend (an in-memory keyring — never the real one) ────────


class _MemKeyring(KeyringBackend):
    priority = 1
    items: dict = {}

    def get_password(self, service, username):
        return self.items.get((service, username))

    def set_password(self, service, username, password):
        self.items[(service, username)] = password

    def delete_password(self, service, username):
        self.items.pop((service, username), None)


@pytest.fixture
def mem_keyring():
    before = keyring.get_keyring()
    ring = _MemKeyring()
    ring.items = {}
    keyring.set_keyring(ring)
    yield ring
    keyring.set_keyring(before)


async def test_the_keychain_backend_keeps_one_item_per_ref(
    seeded_kernel, tmp_path, mem_keyring
):
    k = _mk(seeded_kernel, tmp_path, backend="keychain")
    await _seed(k)
    assert ("fantastic", "creds/openai/main") in mem_keyring.items
    assert not (tmp_path / "creds.yaml").exists(), "no file is written"
    got = await _call(k, {"type": "get", "ref": "openai/main"}, secret=MARK_GET)
    assert got["result"]["fields"] == {"api_key": KEY}
    await _call(k, {"type": "forget", "ref": "openai/main"})
    assert mem_keyring.items == {}


# ─── the env backend (read-only) ─────────────────────────────────────────────


async def test_the_env_backend_reads_named_variables_and_refuses_writes(
    seeded_kernel, tmp_path, monkeypatch
):
    k = _mk(
        seeded_kernel,
        tmp_path,
        backend="env",
        refs={
            "ci/key": {
                "readers": ["alice"],
                "host": "api.example",
                "env": {"api_key": "CI_TEST_KEY"},
            }
        },
    )
    monkeypatch.setenv("CI_TEST_KEY", KEY)
    got = await _call(k, {"type": "get", "ref": "ci/key"}, as_="alice", secret=MARK_GET)
    assert got["result"] == {
        "ref": "ci/key",
        "host": "api.example",
        "fields": {"api_key": KEY},
    }
    w = await _call(
        k,
        {
            "type": "set",
            "ref": "ci/key",
            "host": "api.example",
            "fields": {"api_key": KEY2},
        },
        secret=MARK_SET,
    )
    assert w["result"]["arg"] == "backend", w
    monkeypatch.delenv("CI_TEST_KEY")
    gone = await _call(
        k, {"type": "get", "ref": "ci/key"}, as_="alice", secret=MARK_GET
    )
    assert gone["status"] == "failed" and "$CI_TEST_KEY" in gone["result"]["error"], (
        gone
    )


async def test_a_bad_record_is_refused_at_the_write(seeded_kernel, tmp_path):
    k = seeded_kernel
    r = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "credential_store.tools",
                        "id": "bad",
                        "backend": "vault",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r["reason"] == "invalid_meta" and "backend" in r["error"], r


HOLDS = "holds_secrets must be true — a credential_store always holds secrets"


def _shape(got: str) -> str:
    """The KERNEL's shape refusal — judged before the store's `validate_meta`."""
    return (
        "holds_secrets must be exactly true (this agent holds secrets) or null "
        f"(the operator clears the mark) — got {got}"
    )


async def test_a_store_is_a_secret_holder_and_cannot_stop_being_one(
    seeded_kernel, tmp_path
):
    """The store stamps `holds_secrets: true` on its own record at boot (ai_core
    refuses every model-parsed call to an agent so marked), shows it on reflect,
    and refuses any other value at the write — create and update alike.

    Two checks judge the value, in a FIXED order: the kernel's shape check first
    (anything but `true`/`null` is malformed on every agent — its text), then the
    store's `validate_meta` (it refuses `null`, the operator's clear, on itself —
    the store's text). So `null` gets HOLDS and every other value the kernel's."""
    k = seeded_kernel

    async def op(payload):
        return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]

    made = await op(
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "credential_store.tools",
            "id": "creds",
            "file": str(tmp_path / "creds.yaml"),
        }
    )
    assert made.get("id") == "creds", made
    assert k.get("creds")["holds_secrets"] is True
    shown = (await k.send([{"target": "creds", "payload": {"type": "reflect"}}]))[0]
    assert shown["result"]["holds_secrets"] is True, shown
    for bad, why in (
        (None, HOLDS),
        (False, _shape("bool")),
        ("true", _shape("str")),
        (1, _shape("int")),
    ):
        r = await op({"type": "update_agent", "id": "creds", "holds_secrets": bad})
        assert r == {"error": why, "reason": "invalid_meta", "id": "creds"}, r
    assert k.get("creds")["holds_secrets"] is True
    ok = await op({"type": "update_agent", "id": "creds", "holds_secrets": True})
    assert ok.get("updated") is True, ok
    for bad, why in ((None, HOLDS), (False, _shape("bool"))):
        r = await op(
            {
                "type": "create_agent",
                "parent_id": k.id,
                "handler_module": "credential_store.tools",
                "id": "creds2",
                "holds_secrets": bad,
            }
        )
        assert r == {"error": why, "reason": "invalid_meta"}, r
        assert "creds2" not in k.ctx.agents


async def test_the_file_is_the_kernel_emitters_bytes_sorted(seeded_kernel, tmp_path):
    """ONE format across runtimes: `kernel_state/yaml_io.dump` over sorted refs and
    fields — a value a YAML 1.2 loader would read as a number is quoted, so the rust
    and swift ports read back the same string."""
    k = _mk(seeded_kernel, tmp_path)
    for ref, fields in (
        ("zeta", {"b": "x", "api_key": "1e3"}),
        ("alpha", {"t": "0o17"}),
    ):
        out = await _call(
            k,
            {"type": "set", "ref": ref, "host": "h.example", "fields": fields},
            secret=MARK_SET,
        )
        assert out["result"]["stored"] is True, out
    assert (tmp_path / "creds.yaml").read_text() == (
        "refs:\n  alpha:\n    t: '0o17'\n  zeta:\n    api_key: '1e3'\n    b: x\n"
    )
