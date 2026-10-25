"""file_bridge bundle — filesystem edge of the io family (the verbs, on an OPEN leg)."""

from __future__ import annotations

from _testkit import posix_only


async def _ws(kernel, fb, path, data: bytes, truncate=True):
    return (
        await kernel.send(
            [
                {
                    "target": fb,
                    "payload": {
                        "type": "write_stream",
                        "path": path,
                        "bytes": data,
                        "truncate": truncate,
                    },
                }
            ]
        )
    )[0]["result"]


async def _rs(kernel, fb, path) -> dict:
    """Read the whole file via stream chunks; returns {'content': str} or the error."""
    chunks, offset = [], 0
    while True:
        r = (
            await kernel.send(
                [
                    {
                        "target": fb,
                        "payload": {
                            "type": "read_stream",
                            "path": path,
                            "offset": offset,
                        },
                    }
                ]
            )
        )[0]["result"]
        if not isinstance(r, dict) or r.get("error"):
            return r
        b = r.get("bytes") or b""
        chunks.append(bytes(b))
        if r.get("eof") or not b:
            break
        offset = r.get("next_offset", offset + len(b))
    return {"content": b"".join(chunks).decode()}


async def test_reflect(seeded_kernel, file_bridge):
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert r["sentence"].startswith("Filesystem")
    assert r["readonly"] is False


async def test_write_then_read(seeded_kernel, file_bridge, tmp_path):
    """The STREAM verbs are the only byte path (a program in a JSON string is the
    envelope-escaping failure)."""
    w = await _ws(seeded_kernel, file_bridge, "hello.txt", b"hi there")
    assert w.get("written"), w
    r = await _rs(seeded_kernel, file_bridge, "hello.txt")
    assert r["content"] == "hi there"
    assert (tmp_path / "hello.txt").read_text(encoding="utf-8") == "hi there"


async def test_write_creates_parent_dirs(seeded_kernel, file_bridge, tmp_path):
    w = await _ws(seeded_kernel, file_bridge, "a/b/c.txt", b"deep")
    assert w.get("written"), w
    assert (tmp_path / "a" / "b" / "c.txt").read_text(encoding="utf-8") == "deep"


async def test_read_missing_file_errors(seeded_kernel, file_bridge):
    r = await _rs(seeded_kernel, file_bridge, "nope.txt")
    assert "error" in r


async def test_list(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "a.txt").write_text("a")
    (tmp_path / "b.txt").write_text("b")
    (tmp_path / "subdir").mkdir()
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    names = {f["name"] for f in r["files"]}
    assert {"a.txt", "b.txt", "subdir"} <= names


async def test_list_shows_every_exposed_entry(seeded_kernel, file_bridge, tmp_path):
    """No hidden list: `allow` alone decides exposure, so `allow ["."]` shows the
    dot-entries too — narrowing is the operator's, by naming what to expose."""
    (tmp_path / ".git").mkdir()
    (tmp_path / "visible.txt").write_text("v")
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    names = {f["name"] for f in r["files"]}
    assert ".git" in names
    assert "visible.txt" in names


async def test_delete_file(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "x.txt").write_text("x")
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "delete", "path": "x.txt"}}]
        )
    )[0]["result"]
    assert r["deleted"] is True
    assert not (tmp_path / "x.txt").exists()


async def test_rename(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "old.txt").write_text("o")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": file_bridge,
                    "payload": {
                        "type": "rename",
                        "old_path": "old.txt",
                        "new_path": "new.txt",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r["new_path"] == "new.txt"
    assert (tmp_path / "new.txt").read_text(encoding="utf-8") == "o"


async def test_mkdir(seeded_kernel, file_bridge, tmp_path):
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": file_bridge,
                    "payload": {"type": "mkdir", "path": "newdir/sub"},
                }
            ]
        )
    )[0]["result"]
    assert r["created"] is True
    assert (tmp_path / "newdir" / "sub").is_dir()


async def test_path_safety_rejects_escape(seeded_kernel, file_bridge):
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": file_bridge,
                    "payload": {"type": "read_stream", "path": "../../etc/passwd"},
                }
            ]
        )
    )[0]["result"]
    assert "error" in r
    assert "escape" in r["error"]


async def test_readonly_refuses_write(seeded_kernel, file_bridge):
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": file_bridge,
                        "readonly": True,
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": file_bridge,
                    "payload": {
                        "type": "write_stream",
                        "path": "x.txt",
                        "bytes": b"x",
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in r
    assert "readonly" in r["error"]


async def test_unknown_verb_errors(seeded_kernel, file_bridge):
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "garbage"}}]
        )
    )[0]["result"]
    assert "error" in r


async def test_update_agent_refuses_a_rule_the_bundle_cannot_honour(seeded_kernel):
    """`update_agent` must REFUSE an ingress rule nobody implements.

    Accepting `ingress_rule="password_ingress:kernel-internal-transcripts"` with
    `{"updated": true}` would make the caller believe a security control is in place.
    The edge seals on an unknown rule rather than raising, so it is never unsafe — but
    reporting
    success for a value you cannot honour is its own failure, and the complaint
    belongs at the write, next to the typo."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": ".",
                    },
                }
            ]
        )
    )[0]["result"]
    fid = rec["id"]
    bad = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": fid,
                        "ingress_rule": "password:my-secret",
                    },
                }
            ]
        )
    )[0]["result"]
    assert bad.get("reason") == "invalid_meta"
    assert "password:my-secret" in bad["error"]
    assert "allow_all_ingress" in bad["error"], "must teach the legal set"
    # and the record is UNCHANGED — a refused patch must not half-apply
    assert "ingress_rule" not in (seeded_kernel.get(fid) or {})
    # the real values still go through
    ok = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": fid,
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    assert ok.get("updated") is True
    assert (seeded_kernel.get(fid) or {})["ingress_rule"] == "allow_all_ingress"


# ─── read is WINDOWED (one call cannot flood a conversation) ─────


async def _fb(kernel, tmp_path, **meta) -> str:
    import os

    os.makedirs("wr", exist_ok=True)
    rec = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "wr",
                        "ingress_rule": "allow_all_ingress",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


# ─── argument validation + refusal codes ─────────────────────────────────


async def _call(kernel, fb, **payload):
    return (await kernel.send([{"target": fb, "payload": payload}]))[0]["result"]


async def test_negative_offset_and_zero_length_are_refused_not_fatal(
    seeded_kernel, file_bridge
):
    """`offset`/`length`/`chunk` are validated: a string must not raise out of the
    handler, nor a negative `length` read 0 bytes and make `pump` spin forever.
    Every bad value is `bad_arg` naming the arg."""
    await _ws(seeded_kernel, file_bridge, "a.txt", b"abc")
    for payload in (
        {"type": "read_stream", "path": "a.txt", "offset": -1},
        {"type": "read_stream", "path": "a.txt", "length": 0},
        {"type": "read_stream", "path": "a.txt", "offset": "x"},
        {"type": "read_stream", "path": "a.txt", "offset": True},
        {"type": "write_stream", "path": "a.txt", "bytes": b"z", "offset": -5},
        {"type": "pump", "source_path": "a.txt", "sink_path": "b.txt", "chunk": -1},
        {"type": "pump", "source_path": "a.txt", "sink_path": "b.txt", "chunk": 0},
    ):
        r = await _call(seeded_kernel, file_bridge, **payload)
        assert r.get("reason") == "bad_arg", (payload, r)
        assert r["arg"] in payload, (payload, r)
        assert not r.get("raised"), r
    # and the legal shapes still stream
    r = await _call(
        seeded_kernel, file_bridge, type="read_stream", path="a.txt", offset=1, length=1
    )
    assert bytes(r["bytes"]) == b"b" and r["next_offset"] == 2, r


async def test_an_in_root_symlink_pointing_outside_refuses(
    seeded_kernel, file_bridge, tmp_path
):
    import os

    secret = tmp_path.parent / "sweep_outside_secret.txt"
    secret.write_text("outside")
    os.symlink(secret, tmp_path / "docs")
    r = await _call(seeded_kernel, file_bridge, type="read_stream", path="docs")
    assert r.get("reason") == "transport_error" and "escapes root" in r["error"], r
    r = await _call(seeded_kernel, file_bridge, type="list", path="docs")
    assert r.get("reason") == "transport_error" and "escapes root" in r["error"], r


async def test_bare_delete_refuses_a_populated_dir_recursive_removes_it(
    seeded_kernel, file_bridge, tmp_path
):
    await _ws(seeded_kernel, file_bridge, "src/a.txt", b"x")
    r = await _call(seeded_kernel, file_bridge, type="delete", path="src")
    assert r.get("reason") == "transport_error", r
    assert (tmp_path / "src" / "a.txt").exists()
    r = await _call(
        seeded_kernel, file_bridge, type="delete", path="src", recursive=True
    )
    assert r == {"path": "src", "deleted": True}, r
    assert not (tmp_path / "src").exists()


async def test_empty_path_delete_rename_mkdir_are_refused(
    seeded_kernel, file_bridge, tmp_path
):
    """`delete {path:"", recursive:true}` resolved to the ROOT and removed the
    whole served store. Empty ⇒ `missing_arg`; anything resolving to the root
    itself (`.`, `a/..`) ⇒ `bad_arg`. The root is a boundary, not an operand."""
    await _ws(seeded_kernel, file_bridge, "keep.txt", b"k")
    (tmp_path / "a").mkdir()
    for payload, arg, reason in (
        ({"type": "delete", "path": "", "recursive": True}, "path", "missing_arg"),
        ({"type": "delete", "recursive": True}, "path", "missing_arg"),
        ({"type": "delete", "path": "a/..", "recursive": True}, "path", "bad_arg"),
        (
            {"type": "rename", "old_path": "", "new_path": "x"},
            "old_path",
            "missing_arg",
        ),
        (
            {"type": "rename", "old_path": "a", "new_path": ""},
            "new_path",
            "missing_arg",
        ),
        (
            {"type": "rename", "old_path": ".", "new_path": "x"},
            "old_path",
            "missing_arg",
        ),
        (
            {"type": "rename", "old_path": "a", "new_path": "a/.."},
            "new_path",
            "bad_arg",
        ),
        ({"type": "mkdir", "path": ""}, "path", "missing_arg"),
        ({"type": "mkdir", "path": "."}, "path", "missing_arg"),
    ):
        r = await _call(seeded_kernel, file_bridge, **payload)
        assert r.get("reason") == reason and r.get("arg") == arg, (payload, r)
    assert (tmp_path / "keep.txt").exists() and (tmp_path / "a").is_dir()


async def test_absolute_path_is_refused_with_bad_arg(
    seeded_kernel, file_bridge, tmp_path
):
    """`/README.md` is never `lstrip("/")`-ed and served as `root/README.md`. One
    rule: refuse."""
    await _ws(seeded_kernel, file_bridge, "README.md", b"r")
    for verb, extra in (
        ("read_stream", {"path": "/README.md"}),
        ("list", {"path": "/"}),
        ("delete", {"path": "/README.md"}),
        ("mkdir", {"path": "/x"}),
        ("write_stream", {"path": "/x", "bytes": b"b"}),
        ("rename", {"old_path": "/README.md", "new_path": "y"}),
    ):
        r = await _call(seeded_kernel, file_bridge, type=verb, **extra)
        assert r.get("reason") == "bad_arg", (verb, r)
        assert "root-relative" in r["error"], r
    assert (tmp_path / "README.md").read_bytes() == b"r"


async def test_list_refusals_carry_codes(seeded_kernel, file_bridge):
    await _ws(seeded_kernel, file_bridge, "f.txt", b"f")
    r = await _call(seeded_kernel, file_bridge, type="list", path="nope")
    assert r.get("reason") == "not_found" and r.get("path") == "nope", r
    r = await _call(seeded_kernel, file_bridge, type="list", path="f.txt")
    assert r.get("reason") == "bad_arg" and r.get("arg") == "path", r
    r = await _call(
        seeded_kernel, file_bridge, type="rename", old_path="nope", new_path="x"
    )
    assert r.get("reason") == "not_found" and r.get("path") == "nope", r
    r = await _call(seeded_kernel, file_bridge, type="frobnicate")
    assert r.get("reason") == "unhandled" and r.get("verb") == "frobnicate", r


async def test_reflect_carries_posture_and_boot_is_null(seeded_kernel, file_bridge):
    r = await _call(seeded_kernel, file_bridge, type="reflect")
    assert r["ingress_rule"] == "allow_all_ingress"
    assert r["egress_rule"] == "silent_egress"
    assert r["sealed"] is False
    assert "root_error" not in r
    assert r["sentence"].startswith("Filesystem edge of the io family")
    assert await _call(seeded_kernel, file_bridge, type="boot") is None


async def test_password_ingress_is_refused_on_fs_and_process_legs(seeded_kernel):
    """A leg with no wire envelope can never present a token — `password_ingress`
    on the fs edge could only ever refuse (python) or be satisfied by a secret
    persisted on the record (the ports). Refused at the write, both spellings."""
    for meta in ({"ingress_rule": "password_ingress"}, {"auth": "password"}):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            **meta,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r.get("reason") == "invalid_meta", (meta, r)
        assert "wire envelope" in r["error"], r


# ─── Rust twins (src/lib/rust/crates/bundles/fantastic-file/src/tests.rs) ─────


async def test_write_stream_without_bytes_writes_an_empty_chunk(
    seeded_kernel, file_bridge, tmp_path
):
    """`payload.get("bytes", b"")`: an ABSENT key is an empty chunk (a legal
    write); an explicit non-bytes value is `missing_arg`."""
    w = await _call(
        seeded_kernel, file_bridge, type="write_stream", path="e.txt", truncate=True
    )
    assert w == {"path": "e.txt", "written": 0, "offset": 0, "size": 0}, w
    assert (tmp_path / "e.txt").exists()
    m = await _call(
        seeded_kernel,
        file_bridge,
        type="write_stream",
        path="e2.txt",
        truncate=True,
        bytes=None,
    )
    assert m["reason"] == "missing_arg" and m["arg"] == "bytes (one raw chunk)", m
    assert not (tmp_path / "e2.txt").exists()


async def test_read_stream_reply_key_order(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "a.txt").write_bytes(b"abc")
    r = await _call(seeded_kernel, file_bridge, type="read_stream", path="a.txt")
    assert list(r) == [
        "path",
        "offset",
        "bytes",
        "next_offset",
        "eof",
        "size",
        "origin",
    ], list(r)
    assert bytes(r["bytes"]) == b"abc"
    assert r["origin"] == (
        f"{file_bridge} a.txt bytes 0-3 of 3 — "
        "read_stream {path: a.txt, offset: 0, length: 3}"
    )


@posix_only("symlinks need privilege on Windows")
async def test_dangling_symlink_pointing_outside_root_refuses(
    seeded_kernel, tmp_path
):
    """`realpath(strict=False)` follows a dangling link to where it POINTS;
    outside the root that is an escape, and nothing is created through it. A
    dangling link pointing INSIDE the root is still a deletable name."""
    import os

    root = tmp_path / "wr"
    root.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    os.symlink(outside / "gone.txt", root / "dangling")
    fb = await _fb(seeded_kernel, tmp_path)
    w = await _call(
        seeded_kernel, fb, type="write_stream", path="dangling", bytes=b"x"
    )
    assert w["reason"] == "transport_error", w
    assert w["error"].startswith("fs: path 'dangling' escapes root"), w
    assert not (outside / "gone.txt").exists()
    r = await _call(seeded_kernel, fb, type="read_stream", path="dangling")
    assert r["reason"] == "transport_error", r
    assert r["error"].startswith("fs: path 'dangling' escapes root"), r
    os.symlink(root / "gone.txt", root / "inside")
    d = await _call(seeded_kernel, fb, type="delete", path="inside")
    assert d == {"path": "inside", "deleted": True}, d
    assert not os.path.lexists(root / "inside")


async def test_refusal_texts_use_python_repr(seeded_kernel, tmp_path):
    fb = await _fb(seeded_kernel, tmp_path)
    (tmp_path / "wr" / "f.txt").write_bytes(b"f")
    r = await _call(seeded_kernel, fb, type="list", path="nope")
    assert r["error"] == "path 'nope' does not exist", r
    assert r["reason"] == "not_found" and r["path"] == "nope", r
    r = await _call(seeded_kernel, fb, type="list", path="f.txt")
    assert r["error"] == "list: path must be a directory; 'f.txt' is not one", r
    assert r["reason"] == "bad_arg", r
    r = await _call(seeded_kernel, fb, type="rename", old_path="nope", new_path="x")
    assert r["error"] == "path 'nope' does not exist", r
    assert r["reason"] == "not_found", r
    r = await _call(seeded_kernel, fb, type="delete", path="nope")
    assert r["error"] == "path 'nope' does not exist", r
    r = await _call(seeded_kernel, fb, type="read_stream", path="nope")
    assert r["error"].startswith("file 'nope' not found"), r
    assert r["reason"] == "not_found", r
    # a path holding a quote quotes Python's way
    r = await _call(seeded_kernel, fb, type="list", path="it's")
    assert r["error"] == "path \"it's\" does not exist", r


@posix_only("symlinks need privilege on Windows")
async def test_list_paths_are_relative_to_resolved_root(seeded_kernel, tmp_path):
    """The entry path is relative to the RESOLVED root, and a stat that fails
    reports -1."""
    import os

    root = tmp_path / "wr"
    (root / "x").mkdir(parents=True)
    (root / "sub").mkdir()
    (root / "x" / "a.txt").write_bytes(b"ab")
    os.symlink(root / "x" / "gone.txt", root / "x" / "dangling")
    fb = await _fb(seeded_kernel, tmp_path)
    r = await _call(seeded_kernel, fb, type="list", path="sub/../x")
    files = sorted(r["files"], key=lambda f: f["path"])
    assert [f["path"] for f in files] == ["x/a.txt", "x/dangling"], r
    assert files[0]["size"] == 2
    assert files[1]["size"] == -1, "a dangling link stats as -1"
    r = await _call(seeded_kernel, fb, type="list", path=".")
    assert sorted(f["path"] for f in r["files"]) == ["sub", "x"], r


def test_auth_stem_is_not_judged_as_an_ingress_rule():
    """Only a PRESENT `ingress_rule` is judged against the legal set."""
    from file_bridge.tools import validate_meta

    assert validate_meta({"auth": "nonsense"}, {}) is None
    legal = "allow_all_ingress, deny_ingress"
    assert validate_meta({"ingress_rule": "nonsense"}, {}) == (
        f"ingress_rule='nonsense' is not a rule this bundle implements — "
        f"legal values: {legal}"
    )
    # a rule OBJECT is shown as Python's dict repr
    assert validate_meta({"ingress_rule": {"type": "nope", "env": "X"}}, {}) == (
        "ingress_rule={'type': 'nope', 'env': 'X'} is not a rule this bundle "
        f"implements — legal values: {legal}"
    )
    # a type-less rule object is a plain seal, not a refusal
    assert validate_meta({"ingress_rule": {"env": "X"}}, {}) is None


async def test_reflect_key_order_matches_python(seeded_kernel, file_bridge, tmp_path):
    r = await _call(seeded_kernel, file_bridge, type="reflect")
    assert list(r)[:10] == [
        "id",
        "sentence",
        "root",
        "readonly",
        "allow_count",
        "edit_grammar",
        "compose",
        "ingress_rule",
        "egress_rule",
        "sealed",
    ], list(r)
    assert r["root"] == str(tmp_path.resolve())
    assert r["ingress_rule"] == "allow_all_ingress"
    assert r["egress_rule"] == "silent_egress"
    assert r["sealed"] is False
    assert r["sentence"] == (
        "Filesystem edge of the io family — sealed by default, running-dir only."
    )
    assert await _call(seeded_kernel, file_bridge, type="boot") is None


async def _sealed(kernel, tmp_path, **meta) -> str:
    (tmp_path / "wr").mkdir(exist_ok=True)
    rec = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "root": "wr",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in rec, rec
    return rec["id"]


async def test_unknown_verb_on_sealed_bridge_is_unhandled(seeded_kernel, tmp_path):
    """The verb lookup runs BEFORE the gate: a sealed bridge still says
    "unknown type", not "unauthorized"."""
    from file_bridge import tools

    fb = await _sealed(seeded_kernel, tmp_path)
    r = await _call(seeded_kernel, fb, type="frobnicate")
    assert r == {
        "error": "file_bridge: unknown type 'frobnicate'",
        "reason": "unhandled",
        "verb": "frobnicate",
    }, r
    assert list(r) == ["error", "reason", "verb"]
    # a payload with no `type` never reaches a handler through send; the
    # handler's own rendering is `None`
    r = await tools.handler(fb, {}, seeded_kernel)
    assert r == {
        "error": "file_bridge: unknown type None",
        "reason": "unhandled",
        "verb": None,
    }, r


async def test_sealed_boot_is_denied_and_shutdown_is_unhandled(
    seeded_kernel, tmp_path
):
    """The gate exemption covers `reflect` only."""
    fb = await _sealed(seeded_kernel, tmp_path)
    r = await _call(seeded_kernel, fb, type="boot")
    assert r["reason"] == "unauthorized", r
    r = await _call(seeded_kernel, fb, type="shutdown")
    assert r["reason"] == "unhandled" and r["verb"] == "shutdown", r
    r = await _call(seeded_kernel, fb, type="reflect")
    assert r["sealed"] is True, r


async def test_readonly_wins_over_root_escape(seeded_kernel, tmp_path):
    """The check order is readonly → arguments → root clamp → path clamp."""
    fb = await _fb(seeded_kernel, tmp_path, readonly="yes", root="../outside")
    for verb in ("delete", "rename", "mkdir", "write_stream"):
        r = await _call(seeded_kernel, fb, type=verb)
        assert r["reason"] == "unauthorized", (verb, r)
        assert r["error"] == "agent is readonly", (verb, r)
    # a READ verb on the same bridge reaches the clamp and reports it
    r = await _call(seeded_kernel, fb, type="list")
    assert r["reason"] == "transport_error", r
    assert r["error"] == "fs: root '../outside' escapes the running dir", r
    # a BAD ARGUMENT is answered before the clamp
    r = await _call(seeded_kernel, fb, type="list", path=7)
    assert r["reason"] == "bad_arg", r
    assert r["error"] == "list: path must be a string", r


async def test_text_write_stream_with_string_bytes_is_missing_arg(
    seeded_kernel, file_bridge, tmp_path
):
    """`bytes` IS RAW BYTES: a str, number, None or list is `missing_arg` and
    nothing is written (write/append/edit take text; write_stream does not)."""
    for doc in ("line one\n", 76, None, ["a"]):
        m = await _call(
            seeded_kernel,
            file_bridge,
            type="write_stream",
            path="a.txt",
            truncate=True,
            bytes=doc,
        )
        assert m["reason"] == "missing_arg", (doc, m)
        assert m["arg"] == "bytes (one raw chunk)", (doc, m)
        assert m["error"] == "write_stream: bytes (one raw chunk) required", (doc, m)
        assert not (tmp_path / "a.txt").exists(), f"{doc!r} wrote anyway"


async def test_pump_falsy_args_fall_back_like_python(
    seeded_kernel, file_bridge, tmp_path
):
    """`payload.get(k) or <fallback>` on every pump argument."""
    (tmp_path / "src.txt").write_bytes(b"hello pump")
    # source '' and sink None fall back to this agent; sink_path '' falls back to
    # source_path: a pump onto its own source, refused
    r = await _call(
        seeded_kernel,
        file_bridge,
        type="pump",
        source="",
        sink=None,
        source_path="src.txt",
        sink_path="",
    )
    assert r["reason"] == "bad_arg" and r["arg"] == "sink_path", r
    assert (tmp_path / "src.txt").read_bytes() == b"hello pump"
    # a chunk floor of 1
    r = await _call(
        seeded_kernel,
        file_bridge,
        type="pump",
        source="",
        sink=None,
        source_path="src.txt",
        sink_path="copy.txt",
        chunk=0,
    )
    assert r["reason"] == "bad_arg", r
    # a real copy, chunk by chunk
    r = await _call(
        seeded_kernel,
        file_bridge,
        type="pump",
        source="",
        sink=None,
        source_path="src.txt",
        sink_path="copy.txt",
        chunk=4,
    )
    assert r == {"source": "src.txt", "sink": "copy.txt", "bytes": 10, "chunks": 3}, r
    assert (tmp_path / "copy.txt").read_bytes() == b"hello pump"


async def test_a_readonly_leg_refuses_a_binary_write_before_judging_the_chunk(
    seeded_kernel, tmp_path
):
    """`readonly` is checked FIRST, before the bytes guard: absent or non-bytes
    `bytes` answers `agent is readonly`, never `missing_arg`."""
    fb = await _fb(seeded_kernel, tmp_path, readonly=True)
    for extra in ({"bytes": "x"}, {}, {"bytes": b"data"}):
        out = await _call(
            seeded_kernel, fb, type="write_stream", path="a.txt", **extra
        )
        assert out["error"] == "agent is readonly", (extra, out)
        assert out["reason"] == "unauthorized", (extra, out)
    assert not (tmp_path / "wr" / "a.txt").exists()


async def test_pump_ends_are_ordinary_executions_in_python_shape(
    seeded_kernel, file_bridge, tmp_path
):
    """Each pump end is a ledgered call: the read a plain `{type, path, offset,
    length}`, the write `{type, path, bytes, offset, truncate}`."""
    (tmp_path / "src.txt").write_bytes(b"hello")
    r = await _call(
        seeded_kernel,
        file_bridge,
        type="pump",
        source_path="src.txt",
        sink_path="copy.txt",
    )
    assert r["chunks"] == 1, r
    calls = [
        row["payload"]
        for row in seeded_kernel.ctx.rows.values()
        if row.get("kind") == "call" and not row.get("internal")
    ]
    read = next(p for p in calls if p.get("type") == "read_stream")
    assert list(read) == ["type", "path", "offset", "length"], read
    write = next(p for p in calls if p.get("type") == "write_stream")
    assert list(write) == ["type", "path", "bytes", "offset", "truncate"], write
    assert (tmp_path / "copy.txt").read_bytes() == b"hello"
