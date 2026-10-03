"""credential_store: secrets by ref, bound to a host, read only with a secret mark,
behind per-ref grants (owner, 2026-10-01). Tests never touch the real keychain or
home: the file backend writes under tmp_path, the keychain backend runs on an
in-memory keyring."""

from __future__ import annotations

import json
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


async def _declare(k, refs: dict):
    """Declare refs the ONE way there is: the operator's `update_agent` on the
    store's `refs` — it replaces the map, so the refs already there ride along."""
    have = dict(k.ctx.agents["creds"].record.get("refs") or {})
    out = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "creds",
                        "refs": {**have, **refs},
                    },
                }
            ]
        )
    )[0]["result"]
    assert out.get("updated") is True, out


async def _seed(k):
    await _declare(
        k,
        {
            "openai/main": {
                "host": "api.openai.com",
                "readers": ["alice"],
                "writers": ["bob"],
            }
        },
    )
    out = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "fields": {"api_key": KEY},
        },
        secret=MARK_SET,
    )
    assert out["status"] == "completed", out
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
    unmarked = await _call(k, {"type": "set", "ref": "r", "fields": {"api_key": KEY}})
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


async def test_either_spelling_of_a_mark_counts(seeded_kernel, tmp_path):
    """A mark may be a dotted string or an array of steps (`kernel/_secret`): the
    store accepts either, alone or mixed. A field name holding a dot is named only
    by an array — `result.fields.x.y` is `x` → `y`, which would not mask it."""
    k = _mk(seeded_kernel, tmp_path)
    await _declare(k, {"r1": {"host": "h"}, "r2": {"host": "h"}})
    put = await _call(
        k,
        {"type": "set", "ref": "r1", "fields": {"api_key": KEY}},
        secret=[["payload", "fields"]],
    )
    assert put["status"] == "completed", put
    put = await _call(
        k,
        {
            "type": "set",
            "ref": "r2",
            "fields": {"api_key": KEY, "x.y": KEY2},
        },
        secret=["payload.fields.api_key", ["payload", "fields", "x.y"]],
    )
    assert put["status"] == "completed", put
    got = await _call(k, {"type": "get", "ref": "r1"}, secret=[["result"]])
    assert got["result"]["fields"] == {"api_key": KEY}, got
    got = await _call(
        k,
        {"type": "get", "ref": "r2"},
        secret=[["result", "fields", "api_key"], ["result", "fields", "x.y"]],
    )
    assert got["result"]["fields"] == {"api_key": KEY, "x.y": KEY2}, got
    # the dotted spelling of the dotted field name names `x` → `y`: refused
    bare = await _call(
        k,
        {"type": "get", "ref": "r2"},
        secret=["result.fields.api_key", "result.fields.x.y"],
    )
    assert (bare["result"]["reason"], bare["result"]["arg"]) == ("bad_arg", "secret")
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger) and KEY2 not in repr(ledger), ledger


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


async def test_set_never_creates_a_ref_update_agent_declares_it(
    seeded_kernel, tmp_path
):
    """A ref is declared ONLY by `update_agent` on the store's `refs`. `set` on an
    undeclared ref answers "no such ref" — to an agent AND to the operator — and
    stores nothing; once declared, its writer stores and its reader reads."""
    k = _mk(seeded_kernel, tmp_path)
    put = {"type": "set", "ref": "mine", "fields": {"token": KEY}}
    for who in (None, "maker"):
        nope = await _call(k, put, as_=who, secret=MARK_SET)
        assert nope["result"] == {
            "error": "credential_store: no such ref 'mine'",
            "reason": "not_found",
            "arg": "ref",
        }, (who, nope)
    assert "mine" not in (k.ctx.agents["creds"].record.get("refs") or {})
    assert not (tmp_path / "creds.yaml").exists(), "an undeclared set stores nothing"
    await _declare(
        k, {"mine": {"host": "x.example", "readers": ["maker"], "writers": ["maker"]}}
    )
    made = await _call(k, put, as_="maker", secret=MARK_SET)
    assert made["result"]["stored"] is True, made
    got = await _call(k, {"type": "get", "ref": "mine"}, as_="maker", secret=MARK_GET)
    assert got["result"]["fields"] == {"token": KEY}, got
    # the grants the declaration named are the grants it keeps
    entry = k.ctx.agents["creds"].record["refs"]["mine"]
    assert entry["readers"] == ["maker"] and entry["writers"] == ["maker"], entry
    nope = await _call(k, {**put, "ref": "theirs"}, as_="eve", secret=MARK_SET)
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


async def test_set_takes_no_host_the_declaration_is_the_binding(
    seeded_kernel, tmp_path
):
    """AV-4 (audit 2026-10-03): a ref's host is its DECLARATION — written by
    `update_agent` on the store's `refs`, the one path. `set` stores the value of
    a declared ref and takes no host: a writer naming one is refused, loud, with
    nothing stored and the declared host untouched, so a writer can never rebind
    a ref (and deny every reader at the real host)."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    rebind = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "attacker.example",
            "fields": {"api_key": KEY2},
        },
        as_="bob",
        secret=MARK_SET,
    )
    assert rebind["result"] == {
        "error": "credential_store set: takes no host — a ref's host is its "
        "declaration, written by update_agent on this store's refs",
        "reason": "bad_arg",
        "arg": "host",
    }, rebind
    entry = k.ctx.agents["creds"].record["refs"]["openai/main"]
    assert entry["host"] == "api.openai.com", entry
    assert KEY2 not in (tmp_path / "creds.yaml").read_text(), "nothing stored"
    # the operator too: the host moves only with the declaration
    op = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": {"api_key": KEY2},
        },
        secret=MARK_SET,
    )
    assert op["result"]["arg"] == "host", op
    # a set with no host stores the value under the declared host
    ok = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="bob",
        secret=MARK_SET,
    )
    assert ok["result"] == {
        "ref": "openai/main",
        "host": "api.openai.com",
        "fields": ["api_key"],
        "stored": True,
    }, ok
    got = await _call(
        k,
        {"type": "get", "ref": "openai/main", "host": "https://api.openai.com/v1"},
        as_="alice",
        secret=MARK_GET,
    )
    assert got["result"]["fields"] == {"api_key": KEY2}, got


async def test_a_ref_is_declared_with_its_host(seeded_kernel, tmp_path):
    """The host is the binding, so a declaration without one is refused at the
    write: a value stored under it could be sent nowhere — or anywhere."""
    k = _mk(seeded_kernel, tmp_path)
    r = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "creds",
                        "refs": {"nohost": {"readers": ["alice"]}},
                    },
                }
            ]
        )
    )[0]["result"]
    assert r == {
        "error": "refs['nohost'].host is required — the one host this ref's value "
        "may be sent to",
        "reason": "invalid_meta",
        "id": "creds",
    }, r


async def test_forget_drops_the_value_and_keeps_the_declaration(
    seeded_kernel, tmp_path
):
    """AV-5 (audit 2026-10-03): `forget` drops the VALUE — from the backend, with
    its field names — and nothing else. The declaration (host, readers, writers)
    is the operator's, written and removed by `update_agent` alone, so a writer
    can no longer undeclare a ref nobody could then set again."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    declared = dict(k.ctx.agents["creds"].record["refs"]["openai/main"])
    out = await _call(k, {"type": "forget", "ref": "openai/main"}, as_="bob")
    assert out["result"] == {"forgotten": True, "ref": "openai/main"}
    assert KEY not in (tmp_path / "creds.yaml").read_text()
    entry = k.ctx.agents["creds"].record["refs"]["openai/main"]
    assert entry == {
        "host": declared["host"],
        "readers": declared["readers"],
        "writers": declared["writers"],
    }, entry
    # a reader now finds the ref declared and empty
    empty = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert empty["result"] == {
        "error": "credential_store: ref 'openai/main' has no value in the backend",
        "reason": "not_found",
        "arg": "ref",
    }, empty
    # its writer sets it again, and the reader reads the new value
    back = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="bob",
        secret=MARK_SET,
    )
    assert back["result"]["stored"] is True, back
    got = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert got["result"]["fields"] == {"api_key": KEY2}, got
    # a stranger still sees no ref; undeclaring is update_agent's alone
    nope = await _call(k, {"type": "forget", "ref": "openai/main"}, as_="eve")
    assert nope["result"]["reason"] == "not_found", nope


async def test_the_file_is_judged_on_the_one_open_it_is_read_from(
    seeded_kernel, tmp_path, monkeypatch
):
    """F5 (audit 2026-10-03): the credential file is opened ONCE (`O_NOFOLLOW`)
    and the open descriptor is what is judged (`fstat`: regular, this user's,
    no group/other bits) and read. Checking the path and then reading the path
    again left a window: a leaf swapped for a symlink to another file between
    the two was read as the store's own. Here the swap lands right after any
    path-level look the store takes — the store must never answer the planted
    value."""
    from file_bridge import fs

    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    leaf = tmp_path / "creds.yaml"
    planted = tmp_path / "planted.yaml"
    planted.write_text(
        "refs:\n  openai/main:\n    fields:\n      api_key: sk-PLANTED\n"
        "    host: api.openai.com\n"
    )
    os.chmod(planted, 0o600)
    # the old shape's path check (`fs.lstat`, gone with the fix): swap right after it
    real_lstat = getattr(fs, "lstat", None)

    def lstat_then_swap(*a, **kw):
        st = real_lstat(*a, **kw)
        if not leaf.is_symlink():
            leaf.rename(tmp_path / "moved.yaml")
            leaf.symlink_to(planted)
        return st

    monkeypatch.setattr(fs, "lstat", lstat_then_swap, raising=False)
    got = await _call(k, {"type": "get", "ref": "openai/main"}, secret=MARK_GET)
    assert "sk-PLANTED" not in repr(got), got
    # a symlink in the leaf's place is refused outright, nothing read
    monkeypatch.undo()
    if not leaf.is_symlink():
        leaf.unlink()
        leaf.symlink_to(planted)
    out = await _call(k, {"type": "get", "ref": "openai/main"}, secret=MARK_GET)
    assert out["status"] == "failed", out
    assert (
        "must be a regular file owned by this user with mode 0600"
        in (out["result"]["error"])
    ), out
    assert "sk-PLANTED" not in repr(out), out


async def test_forget_on_the_env_backend_is_refused(seeded_kernel, tmp_path):
    """The env backend is read-only: its value is the variable, so `forget` —
    which drops only a value — has nothing it may drop, and says so."""
    k = _mk(
        seeded_kernel,
        tmp_path,
        backend="env",
        refs={"ci/key": {"host": "api.example", "env": {"api_key": "CI_TEST_KEY"}}},
    )
    out = await _call(k, {"type": "forget", "ref": "ci/key"})
    assert out["result"] == {
        "error": "credential_store forget: the env backend is read-only — unset the "
        "environment variable its ref names",
        "reason": "bad_arg",
        "arg": "backend",
    }, out
    assert "ci/key" in k.ctx.agents["creds"].record["refs"]


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


@pytest.mark.parametrize(
    ("meta", "namespace"),
    [({}, "fantastic"), ({"keychain_namespace": "acme"}, "acme")],
)
async def test_the_keychain_backend_keeps_one_item_per_ref(
    seeded_kernel, tmp_path, mem_keyring, meta, namespace
):
    """One item per ref, under `keychain_namespace` (default `fantastic` — the
    keychain's own word for it is the item's "service"), account `<id>/<ref>`."""
    k = _mk(seeded_kernel, tmp_path, backend="keychain", **meta)
    await _seed(k)
    assert list(mem_keyring.items) == [(namespace, "creds/openai/main")]
    # the item holds the declared host WITH the value (audit S3)
    assert json.loads(mem_keyring.items[(namespace, "creds/openai/main")]) == {
        "host": "api.openai.com",
        "fields": {"api_key": KEY},
    }
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


async def test_per_field_marks_must_cover_every_returned_field(
    seeded_kernel, tmp_path, monkeypatch
):
    """Audit BV-6/S4: the marks are judged against the fields the backend RETURNS,
    not the declared names. An `env` map wider than `fields`, or a value set
    before a narrower re-declaration, returns a field no per-field mark covered —
    refused, and nothing recorded holds either value."""
    k = _mk(
        seeded_kernel,
        tmp_path,
        backend="env",
        refs={
            "ci/key": {
                "readers": ["alice"],
                "host": "api.example",
                "fields": ["api_key"],
                "env": {"api_key": "CI_TEST_KEY", "extra": "CI_TEST_EXTRA"},
            }
        },
    )
    monkeypatch.setenv("CI_TEST_KEY", KEY)
    monkeypatch.setenv("CI_TEST_EXTRA", KEY2)
    one = await _call(
        k,
        {"type": "get", "ref": "ci/key"},
        as_="alice",
        secret=["result.fields.api_key"],
    )
    assert (one["result"]["reason"], one["result"]["arg"]) == ("bad_arg", "secret")
    both = await _call(
        k,
        {"type": "get", "ref": "ci/key"},
        as_="alice",
        secret=["result.fields.api_key", "result.fields.extra"],
    )
    assert both["result"]["fields"] == {"api_key": KEY, "extra": KEY2}, both
    await k.ctx.flush_executions()
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger) and KEY2 not in repr(ledger), ledger


async def test_per_field_marks_after_a_narrower_redeclaration(seeded_kernel, tmp_path):
    """The file variant of BV-6: a writer stored {api_key, extra}; the operator then
    re-declared the ref with fields [api_key]. A get marking only api_key is
    refused — `extra` is still returned by the backend."""
    k = _mk(seeded_kernel, tmp_path)
    await _declare(k, {"r": {"host": "h", "readers": ["alice"]}})
    put = await _call(
        k,
        {"type": "set", "ref": "r", "fields": {"api_key": KEY, "extra": KEY2}},
        secret=MARK_SET,
    )
    assert put["status"] == "completed", put
    entry = dict(k.ctx.agents["creds"].record["refs"]["r"])
    await _declare(k, {"r": {**entry, "fields": ["api_key"]}})
    one = await _call(
        k,
        {"type": "get", "ref": "r"},
        as_="alice",
        secret=["result.fields.api_key"],
    )
    assert (one["result"]["reason"], one["result"]["arg"]) == ("bad_arg", "secret")
    await k.ctx.flush_executions()
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger) and KEY2 not in repr(ledger), ledger


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


def _relative(p: str) -> str:
    return (
        f"file={p!r} must be an absolute path (starting with / or ~) — a relative "
        "path is refused"
    )


async def test_the_file_is_an_absolute_path_a_relative_one_is_refused(
    seeded_kernel, tmp_path
):
    """`file` is `/…` or `~…` — never relative to the kernel's running dir. A
    relative one is refused AT THE WRITE (create and update alike), and a record
    that carries one past the write (an in-process create) is refused at use, loud,
    with nothing read or written."""
    k = seeded_kernel

    async def op(payload):
        return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]

    for rel in ("creds.yaml", "sub/creds.yaml", "./creds.yaml"):
        r = await op(
            {
                "type": "create_agent",
                "parent_id": k.id,
                "handler_module": "credential_store.tools",
                "id": "rel",
                "file": rel,
            }
        )
        assert r == {"error": _relative(rel), "reason": "invalid_meta"}, r
        assert "rel" not in k.ctx.agents
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
    r = await op({"type": "update_agent", "id": "creds", "file": "creds.yaml"})
    assert r == {
        "error": _relative("creds.yaml"),
        "reason": "invalid_meta",
        "id": "creds",
    }
    assert k.get("creds")["file"] == str(tmp_path / "creds.yaml")
    # `~…` passes the write (no disk is touched to judge it)
    from credential_store.tools import validate_meta

    assert validate_meta({"file": "~/.fantastic/credentials/x.yaml"}, {}) is None
    assert validate_meta({"keychain_namespace": ""}, {}) == (
        "keychain_namespace must be a non-empty string"
    )
    # past the write: refused at use, nothing written under the running dir
    k.create("credential_store.tools", id="sneaky", file="creds.yaml")
    k.ctx.update("sneaky", refs={"r": {"host": "h"}})
    out = await _call(
        k,
        {"type": "set", "ref": "r", "fields": {"api_key": KEY}},
        secret=MARK_SET,
        target="sneaky",
    )
    assert out["status"] == "failed", out
    assert out["result"]["error"].endswith(
        f"credential_store: {_relative('creds.yaml')}"
    ), out
    assert not (k.ctx.workdir / "creds.yaml").exists()


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
    """ONE format across runtimes: `kernel_state/yaml_io.dump` with every map's keys
    sorted — each ref's entry is `{fields, host}` (the host stored WITH the value,
    audit S3) — and a value a YAML 1.2 loader would read as a number quoted, so the
    rust and swift ports read back the same string."""
    k = _mk(seeded_kernel, tmp_path)
    await _declare(k, {"zeta": {"host": "h.example"}, "alpha": {"host": "h.example"}})
    for ref, fields in (
        ("zeta", {"b": "x", "api_key": "1e3"}),
        ("alpha", {"t": "0o17"}),
    ):
        out = await _call(
            k,
            {"type": "set", "ref": ref, "fields": fields},
            secret=MARK_SET,
        )
        assert out["result"]["stored"] is True, out
    assert (tmp_path / "creds.yaml").read_text() == (
        "refs:\n"
        "  alpha:\n    fields:\n      t: '0o17'\n    host: h.example\n"
        "  zeta:\n    fields:\n      api_key: '1e3'\n      b: x\n    host: h.example\n"
    )


# ─── the host at rest (audit S3, 2026-10-03) ─────────────────────────────────


# the `get {ref, host}` host-mismatch refusal's shape, reused (owner 2026-10-03)
def _at_rest(ref: str, stored: str | None, declared: str) -> dict:
    return {
        "error": f"credential_store: ref {ref!r} is bound to host {declared!r}, not "
        f"{stored!r} — refused, nothing read",
        "reason": "bad_arg",
        "arg": "host",
    }


_NO_HOST = _at_rest("openai/main", None, "api.openai.com")


async def test_a_redeclared_host_reads_nothing_until_the_value_is_set_again(
    seeded_kernel, tmp_path
):
    """S3: the value is stored WITH the host its declaration named at `set`. The
    operator re-declaring the ref with another host does not carry the value
    there: every `get` is refused, with or without a `host`, and nothing is read —
    until a writer sets the value again, which stores it under the new host."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    entry = dict(k.ctx.agents["creds"].record["refs"]["openai/main"])
    await _declare(k, {"openai/main": {**entry, "host": "attacker.example"}})
    for payload in (
        {"type": "get", "ref": "openai/main"},
        {"type": "get", "ref": "openai/main", "host": "attacker.example"},
    ):
        out = await _call(k, payload, as_="alice", secret=MARK_GET)
        assert out["result"] == _at_rest(
            "openai/main", "api.openai.com", "attacker.example"
        ), out
        assert KEY not in repr(out), out
    back = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="bob",
        secret=MARK_SET,
    )
    assert back["result"]["host"] == "attacker.example", back
    got = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert got["result"] == {
        "ref": "openai/main",
        "host": "attacker.example",
        "fields": {"api_key": KEY2},
    }, got


async def test_another_store_sharing_the_file_reads_nothing_for_its_host(
    seeded_kernel, tmp_path
):
    """S3, the shared-default case: a second store (another project's kernel, or
    one naming the same `file`) declares the SAME ref with another host and a
    reader of its own. The value was stored for the first store's host, so the
    second store's `get` is refused — the value never leaves for its host."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    rec = k.create(
        "credential_store.tools",
        id="other",
        callable_by={"eve": "*"},
        file=str(tmp_path / "creds.yaml"),
        refs={"openai/main": {"host": "blocked.invalid", "readers": ["eve"]}},
    )
    assert rec.get("id") == "other", rec
    k.ctx.agents["eve"].record["can_call"]["other"] = "*"
    out = await _call(
        k,
        {"type": "get", "ref": "openai/main", "host": "blocked.invalid"},
        as_="eve",
        secret=MARK_GET,
        target="other",
    )
    assert out["result"] == _at_rest(
        "openai/main", "api.openai.com", "blocked.invalid"
    ), out
    assert KEY not in repr(out), out


@pytest.mark.parametrize("backend", ["file", "keychain"])
async def test_a_stored_value_without_a_host_is_refused(
    seeded_kernel, tmp_path, mem_keyring, backend
):
    """S3, no back-compat: a value at rest that carries no host — the old
    `{field: value}` shape, or a hand-written entry — is refused, nothing read."""
    k = _mk(seeded_kernel, tmp_path, backend=backend)
    await _declare(k, {"openai/main": {"host": "api.openai.com", "readers": ["alice"]}})
    if backend == "file":
        f = tmp_path / "creds.yaml"
        f.write_text(f"refs:\n  openai/main:\n    api_key: {KEY}\n")
        os.chmod(f, 0o600)
    else:
        mem_keyring.items[("fantastic", "creds/openai/main")] = json.dumps(
            {"api_key": KEY}
        )
    out = await _call(
        k, {"type": "get", "ref": "openai/main"}, as_="alice", secret=MARK_GET
    )
    assert out["result"] == _NO_HOST, out
    assert KEY not in repr(out), out
