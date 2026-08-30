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
