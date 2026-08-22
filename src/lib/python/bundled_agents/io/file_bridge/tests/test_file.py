"""file_bridge bundle — filesystem edge of the io family (the verbs, on an OPEN leg)."""

from __future__ import annotations

import base64


async def test_reflect(seeded_kernel, file_bridge):
    r = await seeded_kernel.send(file_bridge, {"type": "reflect"})
    assert r["sentence"].startswith("Filesystem")
    assert r["readonly"] is False


async def test_write_then_read(seeded_kernel, file_bridge, tmp_path):
    await seeded_kernel.send(
        file_bridge, {"type": "write", "path": "hello.txt", "content": "hi there"}
    )
    r = await seeded_kernel.send(file_bridge, {"type": "read", "path": "hello.txt"})
    assert r["content"] == "hi there"
    assert (tmp_path / "hello.txt").read_text() == "hi there"


async def test_write_creates_parent_dirs(seeded_kernel, file_bridge, tmp_path):
    await seeded_kernel.send(
        file_bridge, {"type": "write", "path": "a/b/c.txt", "content": "deep"}
    )
    assert (tmp_path / "a" / "b" / "c.txt").read_text() == "deep"


async def test_read_missing_file_errors(seeded_kernel, file_bridge):
    r = await seeded_kernel.send(file_bridge, {"type": "read", "path": "nope.txt"})
    assert "error" in r


async def test_read_image_returns_base64(seeded_kernel, file_bridge, tmp_path):
    img_data = b"\x89PNG\r\n\x1a\nfake-png-bytes"
    (tmp_path / "img.png").write_bytes(img_data)
    r = await seeded_kernel.send(file_bridge, {"type": "read", "path": "img.png"})
    assert "image_base64" in r
    assert base64.b64decode(r["image_base64"]) == img_data
    assert r["mime"] == "image/png"


async def test_read_pdf_returns_raw_bytes_with_mime(
    seeded_kernel, file_bridge, tmp_path
):
    """Generic binary (PDF, archive, font, etc.) returns raw bytes +
    mime so the webapp /file/ proxy can serve it as a real download.
    Bytes ride the kernel binary protocol over WS; zero-copy in-process.
    Without this, every non-image binary 404s."""
    pdf_data = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<<>>\nendobj\nxref\n0 1\ntrailer\n<<>>\n%%EOF\n"
    (tmp_path / "doc.pdf").write_bytes(pdf_data)
    r = await seeded_kernel.send(file_bridge, {"type": "read", "path": "doc.pdf"})
    assert "error" not in r, f"PDF read errored: {r}"
    assert isinstance(r.get("bytes"), (bytes, bytearray)), (
        "PDF must return raw bytes, not error / base64"
    )
    assert bytes(r["bytes"]) == pdf_data
    assert r["mime"] == "application/pdf"
    # Sanity: NOT base64 — that would be wasteful + change shape.
    assert "bytes_b64" not in r


async def test_read_unknown_binary_falls_back_to_octet_stream(
    seeded_kernel, file_bridge, tmp_path
):
    """Files with no recognizable extension still come back as bytes —
    just with application/octet-stream as the mime."""
    # Real non-UTF-8 bytes: lone high-bytes that fail strict utf-8 decode.
    # `\x00\x01\x02` are valid utf-8 codepoints — using them would route
    # through the text branch.
    blob = b"\xff\xfe\xfd\xfc binary blob"
    # `.bin` (and many short ones) aren't in mimetypes' DB → octet-stream.
    (tmp_path / "blob.bin").write_bytes(blob)
    r = await seeded_kernel.send(file_bridge, {"type": "read", "path": "blob.bin"})
    assert isinstance(r.get("bytes"), (bytes, bytearray))
    assert bytes(r["bytes"]) == blob
    assert r["mime"] == "application/octet-stream"


async def test_list(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "a.txt").write_text("a")
    (tmp_path / "b.txt").write_text("b")
    (tmp_path / "subdir").mkdir()
    r = await seeded_kernel.send(file_bridge, {"type": "list", "path": ""})
    names = {f["name"] for f in r["files"]}
    assert {"a.txt", "b.txt", "subdir"} <= names


async def test_list_excludes_hidden_default(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / ".git").mkdir()
    (tmp_path / "visible.txt").write_text("v")
    r = await seeded_kernel.send(file_bridge, {"type": "list", "path": ""})
    names = {f["name"] for f in r["files"]}
    assert ".git" not in names
    assert "visible.txt" in names


async def test_delete_file(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "x.txt").write_text("x")
    r = await seeded_kernel.send(file_bridge, {"type": "delete", "path": "x.txt"})
    assert r["deleted"] is True
    assert not (tmp_path / "x.txt").exists()


async def test_rename(seeded_kernel, file_bridge, tmp_path):
    (tmp_path / "old.txt").write_text("o")
    r = await seeded_kernel.send(
        file_bridge,
        {"type": "rename", "old_path": "old.txt", "new_path": "new.txt"},
    )
    assert r["new_path"] == "new.txt"
    assert (tmp_path / "new.txt").read_text() == "o"


async def test_mkdir(seeded_kernel, file_bridge, tmp_path):
    r = await seeded_kernel.send(file_bridge, {"type": "mkdir", "path": "newdir/sub"})
    assert r["created"] is True
    assert (tmp_path / "newdir" / "sub").is_dir()


async def test_path_safety_rejects_escape(seeded_kernel, file_bridge):
    r = await seeded_kernel.send(
        file_bridge, {"type": "read", "path": "../../etc/passwd"}
    )
    assert "error" in r
    assert "escape" in r["error"]


async def test_readonly_refuses_write(seeded_kernel, file_bridge):
    await seeded_kernel.send(
        "kernel_state",
        {"type": "update_agent", "id": file_bridge, "readonly": True},
    )
    r = await seeded_kernel.send(
        file_bridge, {"type": "write", "path": "x.txt", "content": "x"}
    )
    assert "error" in r
    assert "readonly" in r["error"]


async def test_unknown_verb_errors(seeded_kernel, file_bridge):
    r = await seeded_kernel.send(file_bridge, {"type": "garbage"})
    assert "error" in r


async def test_update_agent_refuses_a_rule_the_bundle_cannot_honour(seeded_kernel):
    """`update_agent` must REFUSE an ingress rule nobody implements.

    It used to accept anything: on 2026-08-02 an agent set
    `ingress_rule="password_ingress:kernel-internal-transcripts"` and got `{"updated":
    true}` back, so it believed a security control was in place. The edge now seals
    on an unknown rule rather than raising, so it was never unsafe — but reporting
    success for a value you cannot honour is its own failure, and the complaint
    belongs at the write, next to the typo."""
    rec = await seeded_kernel.send(
        "kernel_state",
        {"type": "create_agent", "handler_module": "file_bridge.tools", "root": "."},
    )
    fid = rec["id"]
    bad = await seeded_kernel.send(
        "kernel",
        {"type": "update_agent", "id": fid, "ingress_rule": "password:my-secret"},
    )
    assert bad.get("updated") is False
    assert "password:my-secret" in bad["error"]
    assert "allow_all_ingress" in bad["error"], "must teach the legal set"
    # and the record is UNCHANGED — a refused patch must not half-apply
    assert "ingress_rule" not in (seeded_kernel.get(fid) or {})
    # the real values still go through
    ok = await seeded_kernel.send(
        "kernel",
        {"type": "update_agent", "id": fid, "ingress_rule": "allow_all_ingress"},
    )
    assert ok.get("updated") is True
    assert (seeded_kernel.get(fid) or {})["ingress_rule"] == "allow_all_ingress"


# ─── read is WINDOWED (one call cannot flood a conversation) ─────


async def _fb(kernel, tmp_path, **meta) -> str:
    import os

    os.makedirs("wr", exist_ok=True)
    rec = await kernel.send(
        kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": "wr",
            "ingress_rule": "allow_all_ingress",
            **meta,
        },
    )
    return rec["id"]


async def test_a_small_read_is_unchanged(seeded_kernel, tmp_path):
    """The window must not alter the ordinary case: a file inside it returns `content`
    and NOTHING else, so every existing caller and assertion is untouched."""
    fid = await _fb(seeded_kernel, tmp_path)
    await seeded_kernel.send(
        fid, {"type": "write", "path": "s.txt", "content": "hello"}
    )
    r = await seeded_kernel.send(fid, {"type": "read", "path": "s.txt"})
    assert r["content"] == "hello"
    assert set(r) == {"path", "content"}, f"window leaked keys on a small read: {r}"


async def test_a_large_read_is_windowed_and_says_so(seeded_kernel, tmp_path):
    """MEASURED 2026-08-07: an AI agent read an 86 KB file, its context reached 21k
    tokens, and the next turn was cancelled mid-prefill — prefill outran the backend's
    180s send timeout. The 32k window was never exceeded, so compaction never fired and
    `context_insufficient` never appeared: the turn became un-completable on TIME while
    legal on SIZE. A read must therefore be bounded, and must SAY it was bounded — a
    caller that cannot tell a whole file from its first page reasons about the wrong
    content."""
    from file_bridge.tools import DEFAULT_MAX_READ_CHARS as CAP

    fid = await _fb(seeded_kernel, tmp_path)
    total = CAP * 2 + 77
    await seeded_kernel.send(
        fid, {"type": "write", "path": "big.txt", "content": "x" * total}
    )
    r = await seeded_kernel.send(fid, {"type": "read", "path": "big.txt"})
    assert len(r["content"]) == CAP
    assert r["truncated"] is True
    assert r["total_chars"] == total and r["next_offset"] == CAP
    assert "offset" in r["hint"], "the hint must name how to continue"


async def test_paging_reassembles_the_whole_file(seeded_kernel, tmp_path):
    """The tail must be REACHABLE, or the window is just data loss with a label."""
    from file_bridge.tools import DEFAULT_MAX_READ_CHARS as CAP

    fid = await _fb(seeded_kernel, tmp_path)
    body = "".join(chr(97 + (i % 26)) for i in range(CAP + 500))
    await seeded_kernel.send(fid, {"type": "write", "path": "p.txt", "content": body})
    out, offset = "", 0
    while True:
        r = await seeded_kernel.send(
            fid, {"type": "read", "path": "p.txt", "offset": offset}
        )
        out += r["content"]
        if not r.get("truncated"):
            break
        offset = r["next_offset"]
    assert out == body


async def test_the_leg_can_widen_its_own_window(seeded_kernel, tmp_path):
    """An operator serving small config files may raise it; `limit` above the leg's cap
    is clamped, so a caller cannot talk its way past the operator's bound."""
    fid = await _fb(seeded_kernel, tmp_path, max_read_chars=64)
    await seeded_kernel.send(
        fid, {"type": "write", "path": "c.txt", "content": "y" * 200}
    )
    r = await seeded_kernel.send(fid, {"type": "read", "path": "c.txt"})
    assert len(r["content"]) == 64 and r["truncated"] is True
    over = await seeded_kernel.send(
        fid, {"type": "read", "path": "c.txt", "limit": 10_000}
    )
    assert len(over["content"]) == 64, "limit must clamp to the leg's cap"
