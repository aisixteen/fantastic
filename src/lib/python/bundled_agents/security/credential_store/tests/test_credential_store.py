"""credential_store: secrets by ref, bound to a host, read by whoever the call gate
admits. Tests never touch the real keychain or
home: the file backend writes under tmp_path, the keychain backend runs on an
in-memory keyring."""

from __future__ import annotations

import json

import keyring
import pytest
from keyring.backend import KeyringBackend

from _testkit import as_agent
from file_bridge import _sys as fb_sys

KEY = "sk-live-0123456789abcdef"
KEY2 = "sk-live-rotated-fedcba98"


async def _call(k, payload, *, as_=None, target="creds"):
    item = {"target": target, "payload": payload}
    if as_ is None:
        return (await k.send([item]))[0]
    with as_agent(k, as_):
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
    assert fb_sys.mode_of(f) == 0o600
    assert KEY in f.read_text(encoding="utf-8")
    rec = k.ctx.agents["creds"].record
    assert KEY not in repr(rec)
    assert rec["refs"]["openai/main"]["host"] == "api.openai.com"
    assert rec["refs"]["openai/main"]["fields"] == ["api_key"]


async def test_any_admitted_caller_reads_and_rotates_a_stranger_is_gated(
    seeded_kernel, tmp_path
):
    """No per-ref access lists: whoever the call gate admits may `get`/`set` a
    declared ref; a caller the store's `callable_by` does not name never reaches
    the handler."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    a = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
    assert a["result"]["fields"] == {"api_key": KEY}, a
    r = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="alice",
    )
    assert r["result"]["stored"] is True, r
    b = await _call(k, {"type": "get", "ref": "openai/main"}, as_="bob")
    assert b["result"]["fields"] == {"api_key": KEY2}, b
    k.create(None, id="mallory", can_call={"creds": "*"})
    m = await _call(k, {"type": "get", "ref": "openai/main"}, as_="mallory")
    assert m["result"]["reason"] == "unauthorized", m
    assert m["result"]["side"] == "ingress", m
    # list is metadata only
    listed = (await _call(k, {"type": "list"}, as_="alice"))["result"]["refs"]
    assert listed == [
        {"ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"]}
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
        nope = await _call(k, put, as_=who)
        assert nope["result"] == {
            "error": "credential_store: no such ref 'mine'",
            "reason": "not_found",
            "arg": "ref",
        }, (who, nope)
    assert "mine" not in (k.ctx.agents["creds"].record.get("refs") or {})
    assert not (tmp_path / "creds.yaml").exists(), "an undeclared set stores nothing"
    await _declare(k, {"mine": {"host": "x.example"}})
    made = await _call(k, put, as_="maker")
    assert made["result"]["stored"] is True, made
    got = await _call(k, {"type": "get", "ref": "mine"}, as_="maker")
    assert got["result"]["fields"] == {"token": KEY}, got
    nope = await _call(k, {**put, "ref": "theirs"}, as_="eve")
    assert nope["result"]["reason"] == "not_found", nope


async def test_set_takes_no_host_the_declaration_is_the_binding(
    seeded_kernel, tmp_path
):
    """A ref's host is its DECLARATION — written by
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
    )
    assert rebind["result"] == {
        "error": "credential_store set: takes no host — a ref's host is its "
        "declaration, written by update_agent on this store's refs",
        "reason": "bad_arg",
        "arg": "host",
    }, rebind
    entry = k.ctx.agents["creds"].record["refs"]["openai/main"]
    assert entry["host"] == "api.openai.com", entry
    assert KEY2 not in (tmp_path / "creds.yaml").read_text(encoding="utf-8"), (
        "nothing stored"
    )
    # the operator too: the host moves only with the declaration
    op = await _call(
        k,
        {
            "type": "set",
            "ref": "openai/main",
            "host": "api.openai.com",
            "fields": {"api_key": KEY2},
        },
    )
    assert op["result"]["arg"] == "host", op
    # a set with no host stores the value under the declared host
    ok = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="bob",
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
                        "refs": {"nohost": {}},
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
    """`forget` drops the VALUE — from the backend, with
    its field names — and nothing else. The declaration (host) is written and
    removed by `update_agent` alone, so a caller can never undeclare a ref nobody
    could then set again."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    declared = dict(k.ctx.agents["creds"].record["refs"]["openai/main"])
    out = await _call(k, {"type": "forget", "ref": "openai/main"}, as_="bob")
    assert out["result"] == {"forgotten": True, "ref": "openai/main"}
    assert KEY not in (tmp_path / "creds.yaml").read_text(encoding="utf-8")
    entry = k.ctx.agents["creds"].record["refs"]["openai/main"]
    assert entry == {"host": declared["host"]}, entry
    # a reader now finds the ref declared and empty
    empty = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
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
    )
    assert back["result"]["stored"] is True, back
    got = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
    assert got["result"]["fields"] == {"api_key": KEY2}, got
    # an undeclared ref is not found; undeclaring is update_agent's alone
    nope = await _call(k, {"type": "forget", "ref": "never/declared"}, as_="eve")
    assert nope["result"]["reason"] == "not_found", nope


async def test_the_file_is_judged_on_the_one_open_it_is_read_from(
    seeded_kernel, tmp_path, monkeypatch
):
    """The credential file is opened ONCE (`O_NOFOLLOW`)
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
    fb_sys.set_mode(planted, 0o600)
    # the old shape's path check (`fs.lstat`, gone with the fix): swap right after it
    real_lstat = getattr(fs, "lstat", None)

    def lstat_then_swap(*a, **kw):
        st = real_lstat(*a, **kw)
        if not leaf.is_symlink():
            leaf.rename(tmp_path / "moved.yaml")
            leaf.symlink_to(planted)
        return st

    monkeypatch.setattr(fs, "lstat", lstat_then_swap, raising=False)
    got = await _call(k, {"type": "get", "ref": "openai/main"})
    assert "sk-PLANTED" not in repr(got), got
    # a symlink in the leaf's place is refused outright, nothing read
    monkeypatch.undo()
    if not leaf.is_symlink():
        leaf.unlink()
        leaf.symlink_to(planted)
    out = await _call(k, {"type": "get", "ref": "openai/main"})
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
    fb_sys.set_mode(tmp_path / "creds.yaml", 0o644)
    out = await _call(k, {"type": "get", "ref": "openai/main"})
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
    # the item holds the declared host WITH the value
    assert json.loads(mem_keyring.items[(namespace, "creds/openai/main")]) == {
        "host": "api.openai.com",
        "fields": {"api_key": KEY},
    }
    assert not (tmp_path / "creds.yaml").exists(), "no file is written"
    got = await _call(k, {"type": "get", "ref": "openai/main"})
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
                "host": "api.example",
                "env": {"api_key": "CI_TEST_KEY"},
            }
        },
    )
    monkeypatch.setenv("CI_TEST_KEY", KEY)
    got = await _call(k, {"type": "get", "ref": "ci/key"}, as_="alice")
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
    )
    assert w["result"]["arg"] == "backend", w
    monkeypatch.delenv("CI_TEST_KEY")
    gone = await _call(k, {"type": "get", "ref": "ci/key"}, as_="alice")
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

    assert validate_meta({"file": "~/.fantastic/user/credentials/x.yaml"}, {}) is None
    assert validate_meta({"keychain_namespace": ""}, {}) == (
        "keychain_namespace must be a non-empty string"
    )
    # past the write: refused at use, nothing written under the running dir
    k.create("credential_store.tools", id="sneaky", file="creds.yaml")
    k.ctx.update("sneaky", refs={"r": {"host": "h"}})
    out = await _call(
        k,
        {"type": "set", "ref": "r", "fields": {"api_key": KEY}},
        target="sneaky",
    )
    assert out["status"] == "failed", out
    assert out["result"]["error"].endswith(
        f"credential_store: {_relative('creds.yaml')}"
    ), out
    assert not (k.ctx.workdir / "creds.yaml").exists()


async def test_the_file_is_pythons_bytes(seeded_kernel, tmp_path):
    """ONE format across runtimes: `kernel_state/yaml_io.dump` with every map's keys
    sorted — each ref's entry is `{fields, host}` (the host stored WITH the
    value) — and a value a YAML 1.2 loader would read as a number quoted, so the
    rust port reads back the same string."""
    k = _mk(seeded_kernel, tmp_path)
    await _declare(k, {"zeta": {"host": "h.example"}, "alpha": {"host": "h.example"}})
    for ref, fields in (
        ("zeta", {"b": "x", "api_key": "1e3"}),
        ("alpha", {"t": "0o17"}),
    ):
        out = await _call(
            k,
            {"type": "set", "ref": ref, "fields": fields},
        )
        assert out["result"]["stored"] is True, out
    assert (tmp_path / "creds.yaml").read_text(encoding="utf-8") == (
        "refs:\n"
        "  alpha:\n    fields:\n      t: '0o17'\n    host: h.example\n"
        "  zeta:\n    fields:\n      api_key: '1e3'\n      b: x\n    host: h.example\n"
    )


# ─── the host at rest ────────────────────────────────────────────────────────


# the `get {ref, host}` host-mismatch refusal's shape, reused
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
        out = await _call(k, payload, as_="alice")
        assert out["result"] == _at_rest(
            "openai/main", "api.openai.com", "attacker.example"
        ), out
        assert KEY not in repr(out), out
    back = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="bob",
    )
    assert back["result"]["host"] == "attacker.example", back
    got = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
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
        refs={"openai/main": {"host": "blocked.invalid"}},
    )
    assert rec.get("id") == "other", rec
    k.ctx.agents["eve"].record["can_call"]["other"] = "*"
    out = await _call(
        k,
        {"type": "get", "ref": "openai/main", "host": "blocked.invalid"},
        as_="eve",
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
    await _declare(k, {"openai/main": {"host": "api.openai.com"}})
    if backend == "file":
        f = tmp_path / "creds.yaml"
        f.write_text(f"refs:\n  openai/main:\n    api_key: {KEY}\n")
        fb_sys.set_mode(f, 0o600)
    else:
        mem_keyring.items[("fantastic", "creds/openai/main")] = json.dumps(
            {"api_key": KEY}
        )
    out = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
    assert out["result"] == _NO_HOST, out
    assert KEY not in repr(out), out


# ─── secrets never reach a row, event or store ──────────────────────────────


def test_field_marks_names_each_field_or_the_whole_map():
    """Twin of the Rust `field_marks_names_each_field_or_the_whole_map`."""
    from credential_store.tools import _field_marks, payload_secret

    assert _field_marks({"api_key": "x", "user": "y"}) == [
        "fields.api_key",
        "fields.user",
    ]
    assert _field_marks({"a.b": "x"}) == ["fields"]
    assert _field_marks({"": "x"}) == ["fields"]
    assert _field_marks("x") == ["fields"]
    assert _field_marks(None) == []
    assert payload_secret({"type": "set", "fields": {"api_key": "x"}}) == [
        "fields.api_key"
    ]
    assert payload_secret({"type": "get", "fields": {"api_key": "x"}}) == []


async def test_concurrent_sets_lose_no_ref(seeded_kernel, tmp_path):
    """Twin of the Rust `concurrent_sets_lose_no_ref`: parallel sets on different
    refs all land in the file."""
    import asyncio

    k = _mk(seeded_kernel, tmp_path)
    refs = [f"r{i:02}" for i in range(16)]
    await _declare(k, {r: {"host": "h.example"} for r in refs})
    outs = await asyncio.gather(
        *[
            _call(k, {"type": "set", "ref": r, "fields": {"api_key": f"k{i}"}})
            for i, r in enumerate(refs)
        ]
    )
    for out in outs:
        assert out["result"]["stored"] is True, out
    assert len(k.ctx.agents["creds"].record["refs"]) == 16
    text = (tmp_path / "creds.yaml").read_text()
    for i, r in enumerate(refs):
        assert f"{r}:" in text and f"k{i}" in text, (r, text)
    for i, r in enumerate(refs):
        got = await _call(k, {"type": "get", "ref": r}, as_="alice")
        assert got["result"]["fields"] == {"api_key": f"k{i}"}, got


async def test_a_set_and_a_get_leave_the_value_in_no_row_event_or_store(
    seeded_kernel, tmp_path
):
    """Twin of the Rust `a_set_and_a_get_leave_the_value_in_no_row_event_or_store`."""
    k = _mk(seeded_kernel, tmp_path)
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "ram_store.tools",
                    "id": "memory",
                },
            }
        ]
    )
    k.ctx.update(k.id, memory_store_id="memory")
    out = await k.ctx.start(boot_all=False)
    assert out.get("started"), out
    events: list = []
    off = k.add_state_subscriber(events.append)
    try:
        await _seed(k)
        with as_agent(k, "alice"):
            got = (
                await k.send(
                    [
                        {
                            "target": "creds",
                            "payload": {"type": "get", "ref": "openai/main"},
                            "id": "get-1",
                        }
                    ]
                )
            )[0]
        assert got["result"]["fields"] == {"api_key": KEY}, got
        await k.ctx.flush_executions()
        dump = str(
            (await k.send([{"target": "memory", "payload": {"type": "dump"}}]))[0][
                "result"
            ]
        )
        ledger = str(
            (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
                "result"
            ]
        )
    finally:
        if callable(off):
            off()
    for name, text in (("store dump", dump), ("ledger", ledger)):
        assert "get-1" in text, name
        assert KEY not in text, name
        assert "[secret]" in text, name
    assert KEY not in repr(events)
    assert KEY not in repr(list(k.ctx.rows.values()))
    assert KEY in (tmp_path / "creds.yaml").read_text()


async def test_a_gate_refused_set_leaves_no_value_in_any_row(seeded_kernel, tmp_path):
    """Twin of the Rust `a_gate_refused_set_leaves_no_value_in_any_row`."""
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    k.create(None, id="stranger", can_call={"creds": "*"})
    refused = await _call(
        k,
        {"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}},
        as_="stranger",
    )
    assert refused["status"] != "completed", refused
    assert refused["result"]["reason"] == "unauthorized", refused
    assert KEY2 not in repr(refused)
    assert KEY2 not in repr(list(k.ctx.rows.values()))
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY2 not in repr(ledger)
    assert KEY2 not in (tmp_path / "creds.yaml").read_text()
    got = await _call(k, {"type": "get", "ref": "openai/main"}, as_="alice")
    assert got["result"]["fields"] == {"api_key": KEY}, got


def test_the_default_file_is_under_the_homes_user_credentials(monkeypatch, tmp_path):
    """No `file`: `<home>/user/credentials/<id>.yaml`, `<home>` being
    FANTASTIC_HOME (made absolute), else `~/.fantastic`."""
    from credential_store.tools import default_file

    monkeypatch.delenv("FANTASTIC_HOME", raising=False)
    assert default_file("c2") == "~/.fantastic/user/credentials/c2.yaml"
    monkeypatch.setenv("FANTASTIC_HOME", str(tmp_path / "h"))
    assert default_file("c2") == f"{tmp_path / 'h'}/user/credentials/c2.yaml"
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FANTASTIC_HOME", "rel")
    assert default_file("c2") == f"{tmp_path / 'rel'}/user/credentials/c2.yaml"


# ─── Rust twins (#170) ───────────────────────────────────────────────────────


async def test_a_read_pinned_to_another_host_is_refused(seeded_kernel, tmp_path):
    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    bad = (
        await _call(
            k,
            {"type": "get", "ref": "openai/main", "host": "https://evil.example.net/v1"},
        )
    )["result"]
    assert bad == {
        "error": "credential_store: ref 'openai/main' is bound to host "
        "'api.openai.com', not 'evil.example.net' — refused, nothing read",
        "reason": "bad_arg",
        "arg": "host",
    }
    assert list(bad) == ["error", "reason", "arg"]
    good = (
        await _call(
            k,
            {
                "type": "get",
                "ref": "openai/main",
                "host": "HTTPS://API.OpenAI.com/v1/responses",
            },
        )
    )["result"]
    assert good["fields"] == {"api_key": KEY}, good
    # a host that names nothing is no host
    blank = (
        await _call(k, {"type": "get", "ref": "openai/main", "host": "  "})
    )["result"]
    assert blank["error"].endswith("not None — refused, nothing read"), blank


def test_host_is_pythons_urlsplit_netloc_lowercased():
    from credential_store.tools import _host

    assert _host("https://api.openai.com/v1") == "api.openai.com"
    assert _host("http://127.0.0.1:8080/x?y") == "127.0.0.1:8080"
    assert _host("  API.Example.COM  ") == "api.example.com"
    assert _host("://nohost") is None
    assert _host("1http://x/y") is None
    assert _host("") is None
    assert _host(5) is None
    assert _host(None) is None


def test_validate_meta_texts_are_pythons():
    from credential_store.tools import validate_meta

    def v(m):
        return validate_meta(m, {})

    assert v({"backend": None}) is None
    assert v({"file": " "}) == "file must be a non-empty string"
    assert (
        v({"keychain_namespace": 3}) == "keychain_namespace must be a non-empty string"
    )
    assert v({"file": " creds.yaml"}) == (
        "file=' creds.yaml' must be an absolute path (starting with / or ~) — a "
        "relative path is refused"
    )
    assert v({"file": "/abs/creds.yaml"}) is None
    assert v({"refs": []}) == "refs must be a map of ref to {host, fields, env}"
    assert v({"refs": {" ": {}}}) == "a ref must be a non-empty string"
    assert v({"refs": {"r": 1}}) == "refs['r'] must be a map"
    assert (
        v({"refs": {"r": {"host": "://"}}}) == "refs['r'].host must be a non-empty host"
    )
    for nohost in ({}, {"host": None}):
        assert v({"refs": {"r": nohost}}) == (
            "refs['r'].host is required — the one host this ref's value may be "
            "sent to"
        )
    assert (
        v({"refs": {"r": {"host": "h", "env": {"api_key": ""}}}})
        == "refs['r'].env must map field names to variable names"
    )
    assert (
        v({"refs": {"r": {"host": "h", "fields": ["k"], "env": {"k": "V"}}}}) is None
    )


async def test_reflect_is_posture_never_a_value_or_a_ref_name(seeded_kernel, tmp_path):
    from credential_store.tools import VERBS, default_file

    k = _mk(seeded_kernel, tmp_path)
    await _seed(k)
    r = (await _call(k, {"type": "reflect"}))["result"]
    assert list(r)[:5] == ["id", "sentence", "backend", "refs", "file"], r
    assert r["backend"] == "file"
    assert r["refs"] == 1
    assert r["file"] == str(tmp_path / "creds.yaml")
    assert list(VERBS) == ["reflect", "boot", "set", "get", "list", "forget"]
    text = json.dumps(r)
    assert KEY not in text and "openai/main" not in text
    # with no `file`, the default under the user's home is NAMED, never touched
    k.create("credential_store.tools", id="c2")
    r2 = (await _call(k, {"type": "reflect"}, target="c2"))["result"]
    assert r2["file"] == default_file("c2")


async def test_missing_args_and_unknown_verbs_are_refused_in_pythons_words(
    seeded_kernel, tmp_path
):
    k = _mk(seeded_kernel, tmp_path)
    for payload, err in (
        (
            {"type": "set", "host": "h", "fields": {"k": "v"}},
            "credential_store set: ref required",
        ),
        ({"type": "get", "ref": " "}, "credential_store get: ref required"),
        ({"type": "forget"}, "credential_store forget: ref required"),
    ):
        out = (await _call(k, payload))["result"]
        assert out == {"error": err, "reason": "missing_arg", "arg": "ref"}, out
        assert list(out) == ["error", "reason", "arg"]
    bad = (await _call(k, {"type": "set", "ref": "r", "fields": {"k": 1}}))["result"]
    assert bad == {
        "error": "credential_store set: fields must be a non-empty map of field name "
        "to string value",
        "reason": "bad_arg",
        "arg": "fields",
    }
    unk = (await _call(k, {"type": "shutdown"}))["result"]
    assert unk == {
        "error": "credential_store: unknown type 'shutdown'",
        "reason": "unhandled",
        "verb": "shutdown",
    }
