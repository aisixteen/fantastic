"""file_bridge bundle — filesystem edge of the io family (the verbs, on an OPEN leg)."""

from __future__ import annotations


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
    """The STREAM verbs are the only byte path — the string `read`/`write` died
    (a program in a JSON string is the measured envelope-escaping failure)."""
    w = await _ws(seeded_kernel, file_bridge, "hello.txt", b"hi there")
    assert w.get("written"), w
    r = await _rs(seeded_kernel, file_bridge, "hello.txt")
    assert r["content"] == "hi there"
    assert (tmp_path / "hello.txt").read_text() == "hi there"


async def test_write_creates_parent_dirs(seeded_kernel, file_bridge, tmp_path):
    w = await _ws(seeded_kernel, file_bridge, "a/b/c.txt", b"deep")
    assert w.get("written"), w
    assert (tmp_path / "a" / "b" / "c.txt").read_text() == "deep"


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


async def test_list_excludes_hidden_default(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / ".git").mkdir()
    (tmp_path / "visible.txt").write_text("v")
    r = (
        await seeded_kernel.send(
            [{"target": file_bridge, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    names = {f["name"] for f in r["files"]}
    assert ".git" not in names
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
    assert (tmp_path / "new.txt").read_text() == "o"


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

    It used to accept anything: on 2026-08-02 an agent set
    `ingress_rule="password_ingress:kernel-internal-transcripts"` and got `{"updated":
    true}` back, so it believed a security control was in place. The edge now seals
    on an unknown rule rather than raising, so it was never unsafe — but reporting
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
                        "root": "wr",
                        "ingress_rule": "allow_all_ingress",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


# ─── the 2026-09-02 closing sweep — argument validation + refusal codes ──


async def _call(kernel, fb, **payload):
    return (await kernel.send([{"target": fb, "payload": payload}]))[0]["result"]


async def test_negative_offset_and_zero_length_are_refused_not_fatal(
    seeded_kernel, file_bridge
):
    """`offset`/`length`/`chunk` used to be `int()`-coerced OUTSIDE the try: a
    string raised out of the handler, a negative `length` read 0 bytes and made
    `pump` spin forever. Now every bad value is `bad_arg` naming the arg."""
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
    """Python used to `lstrip("/")` and serve `/README.md` as `root/README.md`,
    against its own docstring; the ports refused. One rule now: refuse."""
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
                            **meta,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r.get("reason") == "invalid_meta", (meta, r)
        assert "wire envelope" in r["error"], r
