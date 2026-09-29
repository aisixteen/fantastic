"""file_bridge's TEXT verbs — `read` / `write` / `edit` / `append` / `undo`.

A caller edits with its own grants on the bridge, so there is no deputy. The
posture cases (readonly, sealed) are answered by the bridge's own gate. `undo` is one RAM slot per path — a second undo is redo.
"""

from __future__ import annotations

import asyncio
import hashlib


async def _mk(kernel, *, fb_meta=None):
    """One open file_bridge, returned twice (`fb, ed`): the editor IS the bridge."""
    fb = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "sd",
                        "ingress_rule": "allow_all_ingress",
                        **(fb_meta or {}),
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    return fb, fb


async def _call(kernel, ed, payload):
    return (await kernel.send([{"target": ed, "payload": payload}]))[0]["result"]


async def _patch(kernel, ed, path, patch, *, hunks=None, if_match=None, **extra):
    """One-file edit through the TRANSACTION shape: even a single file rides
    the `edits` array (one shape, no drift)."""
    edit = {
        "path": path,
        "hunks": hunks
        if hunks is not None
        else max(1, str(patch).count("<<<<<<< SEARCH")),
    }
    if if_match is not None:
        edit["if_match"] = if_match
    payload = {"type": "edit", "edits": [edit], **extra}
    if patch is not None:
        payload["bytes"] = patch.encode("utf-8") if isinstance(patch, str) else patch
    return await _call(kernel, ed, payload)


def _block(old, new):
    return f"<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE"


async def _seed(kernel, ed, path, text):
    return await _call(
        kernel, ed, {"type": "write", "path": path, "bytes": text.encode("utf-8")}
    )


# ─── patch: happy paths (ported from the file_bridge suite) ─────


async def test_single_hunk_applies_and_writes(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\nb = 2\nc = 3\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("b = 2", "b = 20"))
    assert r["edited"] is True and r["edits"][0]["hunks"] == 1
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 20\nc = 3\n"


async def test_multiple_hunks_apply_in_order(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "x = 1\ny = 2\nz = 3\n")
    patch = _block("x = 1", "x = 11") + "\n" + _block("z = 3", "z = 33")
    r = await _patch(seeded_kernel, ed, "m.py", patch)
    assert r["edited"] is True and r["edits"][0]["hunks"] == 2
    assert (tmp_path / "sd" / "m.py").read_text() == "x = 11\ny = 2\nz = 33\n"


async def test_hunk_matches_content_a_prior_hunk_created(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.txt", "one\n")
    patch = _block("one", "two") + "\n" + _block("two", "three")
    r = await _patch(seeded_kernel, ed, "m.txt", patch)
    assert r["edited"] is True and r["edits"][0]["hunks"] == 2
    assert (tmp_path / "sd" / "m.txt").read_text() == "three\n"


async def test_hunk_with_quotes_backslashes_newlines_is_verbatim(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    original = 'print("hi")\npath = "c:\\\\tmp"\n'
    await _seed(seeded_kernel, ed, "p.py", original)
    new = 'print("bye", end="")\npath = "c:\\\\tmp"\n'
    r = await _patch(
        seeded_kernel, ed, "p.py", _block('print("hi")', 'print("bye", end="")')
    )
    assert r["edited"] is True
    assert (tmp_path / "sd" / "p.py").read_text() == new


async def test_unicode_content_and_hunk(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "u.txt", "café — over\nnext\n")
    r = await _patch(seeded_kernel, ed, "u.txt", _block("café — over", "tea — done"))
    assert r["edited"] is True
    assert (tmp_path / "sd" / "u.txt").read_text() == "tea — done\nnext\n"


async def test_multiline_hunk(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(
        seeded_kernel, ed, "m.py", "def f():\n    return 1\n\ndef g():\n    return 2\n"
    )
    r = await _patch(
        seeded_kernel,
        ed,
        "m.py",
        _block("def f():\n    return 1", "def f():\n    return 99"),
    )
    assert r["edited"] is True
    assert (tmp_path / "sd" / "m.py").read_text() == (
        "def f():\n    return 99\n\ndef g():\n    return 2\n"
    )


# ─── patch: fail-loud, all-or-nothing ───────────────────────────


async def test_no_match_fails_and_writes_nothing(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("nope = 0", "x = 0"))
    assert "edited" not in r
    assert r["reason"] == "no_match"
    assert r["failed"][0]["index"] == 0
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_ambiguous_match_fails_and_writes_nothing(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "v = 1\nv = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("v = 1", "v = 2"))
    assert "edited" not in r
    assert r["reason"] == "ambiguous"
    assert "2 places" in r["failed"][0]["reason"]
    assert (tmp_path / "sd" / "m.py").read_text() == "v = 1\nv = 1\n"


async def test_second_hunk_failure_reverts_the_whole_patch(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\nb = 2\n")
    patch = _block("a = 1", "a = 10") + "\n" + _block("missing", "x")
    r = await _patch(seeded_kernel, ed, "m.py", patch)
    assert "edited" not in r and r["failed"][0]["index"] == 1
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 2\n"


# ─── patch: bad input ───────────────────────────────────────────


async def test_malformed_patch_unterminated_block(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    bad_patch = "<<<<<<< SEARCH\na = 1\n=======\na = 2\n"  # no >>>>>>> REPLACE
    r = await _patch(seeded_kernel, ed, "m.py", bad_patch)
    assert "edited" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_empty_patch_no_blocks(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", "just some prose, no markers")
    assert "edited" not in r and r["reason"] == "bad_arg"


async def test_missing_bytes(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", None)
    assert "edited" not in r and "bytes" in r["error"]


async def test_file_not_found(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _patch(seeded_kernel, ed, "nope.py", _block("a", "b"))
    assert "edited" not in r and r["reason"] == "not_found"


async def test_missing_path(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _call(
        seeded_kernel, ed, {"type": "edit", "bytes": _block("a", "b").encode("utf-8")}
    )
    assert "edited" not in r and "edits" in r["error"]


async def test_bad_utf8_patch(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", b"\xff\xfe\xff")
    assert "edited" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_malformed_missing_divider(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    bad_patch = "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE"
    r = await _patch(seeded_kernel, ed, "m.py", bad_patch)
    assert "edited" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


# ─── the wired LEG refuses (was: readonly / sealed on the bridge) ────


async def test_readonly_leg_refuses_the_write(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel, fb_meta={"readonly": True})
    (tmp_path / "sd").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / "m.py").write_text("a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("a = 1", "a = 2"))
    assert r["reason"] == "unauthorized"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_sealed_leg_denies_everything(seeded_kernel, tmp_path, monkeypatch):
    # The leg has no ingress_rule ⇒ sealed by default. The editor's send is
    # answered by the LEG's gate; the editor passes the refusal through.
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel, fb_meta={"ingress_rule": None})
    (tmp_path / "sd").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / "m.py").write_text("a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("a = 1", "a = 2"))
    assert r["reason"] == "unauthorized"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


# ─── read / write / append / version ────────────────────────────


async def test_write_then_read_lines(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    w = await _seed(seeded_kernel, ed, "doc.md", "alpha\nbeta\ngamma\n")
    assert w["size"] == 17 and len(w["version"]) == 16
    r = await _call(seeded_kernel, ed, {"type": "read", "path": "doc.md"})
    assert [ln["text"] for ln in r["lines"]] == ["alpha", "beta", "gamma"]
    assert [ln["n"] for ln in r["lines"]] == [1, 2, 3]
    assert r["total"] == 3 and r["eof"] is True
    assert r["version"] == w["version"]


async def test_read_pages_by_line_offset(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "n.txt", "".join(f"L{i}\n" for i in range(10)))
    r = await _call(
        seeded_kernel, ed, {"type": "read", "path": "n.txt", "offset": 4, "limit": 3}
    )
    assert [ln["text"] for ln in r["lines"]] == ["L4", "L5", "L6"]
    assert r["lines"][0]["n"] == 5 and r["eof"] is False and r["total"] == 10


async def test_read_refuses_binary(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _call(
        seeded_kernel,
        ed,
        {"type": "write", "path": "b.bin", "bytes": b"a\x00b"},
    )
    r = await _call(seeded_kernel, ed, {"type": "read", "path": "b.bin"})
    assert r["reason"] == "bad_arg" and "binary" in r["error"]


async def test_append_creates_then_grows(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r1 = await _call(
        seeded_kernel, ed, {"type": "append", "path": "log.txt", "bytes": b"one\n"}
    )
    assert r1["size"] == 4
    r2 = await _call(
        seeded_kernel, ed, {"type": "append", "path": "log.txt", "bytes": b"two\n"}
    )
    assert r2["size"] == 8
    assert (tmp_path / "sd" / "log.txt").read_text() == "one\ntwo\n"
    assert r1["version"] != r2["version"]


async def test_write_is_atomic_no_tmp_debris_on_success(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "x\n")
    names = [p.name for p in (tmp_path / "sd").iterdir()]
    assert names == ["a.txt"]  # the tmp sibling was renamed away


async def test_if_match_guards_write_and_patch(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    w = await _seed(seeded_kernel, ed, "v.txt", "one\n")
    # stale guard refuses, names both versions
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "write", "path": "v.txt", "bytes": b"two\n", "if_match": "0" * 16},
    )
    assert r["reason"] == "version_mismatch" and r["actual"] == w["version"]
    assert (tmp_path / "sd" / "v.txt").read_text() == "one\n"
    # current guard passes
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "write",
            "path": "v.txt",
            "bytes": b"two\n",
            "if_match": w["version"],
        },
    )
    assert r["version"] != w["version"]
    # patch honours the same guard
    r = await _patch(
        seeded_kernel, ed, "v.txt", _block("two", "three"), if_match="0" * 16
    )
    assert r["reason"] == "version_mismatch" and r["path"] == "v.txt"


async def test_patch_dry_run_writes_nothing(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "d.txt", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "d.txt", _block("a = 1", "a = 2"), dry_run=True)
    assert r["dry_run"] is True and r["edits"][0]["hunks"] == 1
    assert (tmp_path / "sd" / "d.txt").read_text() == "a = 1\n"


# ─── checkpoints ────────────────────────────────────────────────


# ─── unwired failfast + `on` override ───────────────────────────


# ─── the no-bypass proof ────────────────────────────────────────


# ─── the TRANSACTION (multi-file edits in ONE patch) ────────────


async def test_multifile_patch_applies_both_atomically(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.py", "x = 1\n")
    await _seed(seeded_kernel, ed, "b.py", "y = 2\nz = 3\n")
    blob = (
        _block("x = 1", "x = 10")
        + "\n"
        + _block("y = 2", "y = 20")
        + "\n"
        + _block("z = 3", "z = 30")
    )
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 2}],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert r["edited"] is True, r
    assert [(e["path"], e["hunks"]) for e in r["edits"]] == [("a.py", 1), ("b.py", 2)]
    assert (tmp_path / "sd" / "a.py").read_text() == "x = 10\n"
    assert (tmp_path / "sd" / "b.py").read_text() == "y = 20\nz = 30\n"
    ck = tmp_path / "sd" / ".ckpt"
    assert not ck.exists() or not any(f.is_file() for f in ck.rglob("*")), (
        "a committed transaction must self-clean its pre-images"
    )


async def test_multifile_doomed_second_edit_writes_nothing(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.py", "x = 1\n")
    await _seed(seeded_kernel, ed, "b.py", "y = 2\n")
    blob = _block("x = 1", "x = 10") + "\n" + _block("missing", "nope")
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert "edited" not in r and r["reason"] == "no_match", r
    assert r["failed"][0]["path"] == "b.py"
    assert (tmp_path / "sd" / "a.py").read_text() == "x = 1\n"
    assert (tmp_path / "sd" / "b.py").read_text() == "y = 2\n"


async def test_hunk_counts_must_sum_to_the_blob(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.py", "x = 1\n")
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": "a.py", "hunks": 2}],
            "bytes": _block("x = 1", "x = 2").encode("utf-8"),
        },
    )
    assert r["reason"] == "bad_arg" and "sum" in r["error"], r
    assert (tmp_path / "sd" / "a.py").read_text() == "x = 1\n"


async def test_per_edit_if_match_guards_the_whole_transaction(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.py", "one\n")
    await _seed(seeded_kernel, ed, "b.py", "two\n")
    blob = _block("one", "ONE") + "\n" + _block("two", "TWO")
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [
                {"path": "a.py", "hunks": 1},
                {"path": "b.py", "hunks": 1, "if_match": "0" * 16},
            ],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert r["reason"] == "version_mismatch" and r["path"] == "b.py", r
    assert (tmp_path / "sd" / "a.py").read_text() == "one\n"


async def test_write_accepts_str_and_utf8_encodes(seeded_kernel, tmp_path, monkeypatch):
    """Text courtesy: ai_core encodes for the MODEL; a spawned program's ctrl-fd
    wire is JSON (no bytes type) — a str document must land verbatim utf-8
    (measured 2026-09-01, E20: the refusal drove a model to improvise base64)."""
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _call(
        seeded_kernel, ed, {"type": "write", "path": "t.txt", "bytes": "h\u00e9llo\n"}
    )
    assert r.get("size") == len("h\u00e9llo\n".encode()), r
    assert (tmp_path / "sd" / "t.txt").read_text() == "h\u00e9llo\n"


# ─── per-document serialization + payload hardening (sweep I-5) ──


async def test_patch_refuses_the_same_path_twice(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.md", "one\ntwo\n")
    blob = _block("one", "1") + "\n" + _block("two", "2")
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": "a.md", "hunks": 1}, {"path": "a.md", "hunks": 1}],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert r["reason"] == "bad_arg" and r["arg"] == "edits", r
    assert "at most once" in r["error"]
    assert (tmp_path / "sd" / "a.md").read_text() == "one\ntwo\n"


async def test_concurrent_patches_on_one_file_both_land(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "f.py", "a = 1\nb = 2\n")
    r1, r2 = await asyncio.gather(
        _patch(seeded_kernel, ed, "f.py", _block("a = 1", "a = 10")),
        _patch(seeded_kernel, ed, "f.py", _block("b = 2", "b = 20")),
    )
    assert r1["edited"] is True and r2["edited"] is True, (r1, r2)
    final = (tmp_path / "sd" / "f.py").read_bytes()
    assert final == b"a = 10\nb = 20\n", final
    # each reply's version is the document as it stood at THAT commit: one is
    # the final image, the other is the intermediate with only its own hunk.
    v_final = hashlib.sha256(final).hexdigest()[:16]
    versions = {r1["edits"][0]["version"], r2["edits"][0]["version"]}
    assert v_final in versions, (versions, v_final)
    intermediates = {
        hashlib.sha256(b"a = 10\nb = 2\n").hexdigest()[:16],
        hashlib.sha256(b"a = 1\nb = 20\n").hexdigest()[:16],
    }
    assert versions - {v_final} <= intermediates, versions


async def test_text_channel_bytes_guard_is_missing_arg(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    for verb in ("write", "append"):
        r = await _call(seeded_kernel, ed, {"type": verb, "path": "x"})
        assert r["reason"] == "missing_arg" and r["arg"].startswith("bytes"), r
    r = await _call(
        seeded_kernel, ed, {"type": "edit", "edits": [{"path": "x", "hunks": 1}]}
    )
    assert r["reason"] == "missing_arg" and r["arg"].startswith("bytes"), r


async def test_unknown_type_carries_verb(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _call(seeded_kernel, ed, {"type": "frobnicate"})
    assert r["reason"] == "unhandled" and r["verb"] == "frobnicate", r


async def test_read_with_max_limit_does_not_trap(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "r.txt", "l1\nl2\n")
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "read", "path": "r.txt", "offset": 1, "limit": 2**63 - 1},
    )
    assert r["eof"] is True and [x["text"] for x in r["lines"]] == ["l2"], r


async def test_non_int_args_are_bad_arg_refusals(seeded_kernel, tmp_path, monkeypatch):
    """#136 — `read {offset: "ten"}` used to raise ValueError out of the handler.
    A present-but-wrong number is a typo, and it is answered as one; an explicit
    null is ABSENT, as on every other verb of the bridge."""
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "n.txt", "".join(f"L{i}\n" for i in range(10)))
    for bad_value in ("ten", 1.5, True, -1, [0]):
        r = await _call(
            seeded_kernel, ed, {"type": "read", "path": "n.txt", "offset": bad_value}
        )
        assert r.get("reason") == "bad_arg" and r.get("arg") == "offset", (bad_value, r)
    for bad_value in ("200", 2.0, True, 0, -1):
        r = await _call(
            seeded_kernel, ed, {"type": "read", "path": "n.txt", "limit": bad_value}
        )
        assert r.get("reason") == "bad_arg" and r.get("arg") == "limit", (bad_value, r)
    # ABSENT still takes the documented defaults (offset 0, limit 200).
    r = await _call(seeded_kernel, ed, {"type": "read", "path": "n.txt"})
    assert [ln["text"] for ln in r["lines"]] == [f"L{i}" for i in range(10)]
    assert r["lines"][0]["n"] == 1 and r["eof"] is True, r


async def test_patch_hunk_count_must_be_an_int(seeded_kernel, tmp_path, monkeypatch):
    """#136 — the per-edit `hunks` count is typed too: a string count would slice
    the shared blob by nothing at all."""
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    blob = _block("a = 1", "a = 2").encode("utf-8")
    for bad_value in ("1", 1.5, True, None, 0, -1):
        r = await _call(
            seeded_kernel,
            ed,
            {
                "type": "edit",
                "edits": [{"path": "m.py", "hunks": bad_value}],
                "bytes": blob,
            },
        )
        assert r.get("reason") == "bad_arg" and r.get("arg") == "edits", (bad_value, r)
    # …and the file was not touched by any of them.
    r = await _call(seeded_kernel, ed, {"type": "read", "path": "m.py"})
    assert [ln["text"] for ln in r["lines"]] == ["a = 1"], r


# ─── undo: ONE slot per path, in RAM ────────────────────────────


async def test_undo_restores_what_the_last_write_replaced_and_redoes(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "u.txt", "one\n")
    await _seed(seeded_kernel, ed, "u.txt", "two\n")
    r = await _call(seeded_kernel, ed, {"type": "undo", "path": "u.txt"})
    assert r["undone"] is True, r
    assert (tmp_path / "sd" / "u.txt").read_text() == "one\n"
    # a second undo is REDO: the slot now holds what the undo replaced
    await _call(seeded_kernel, ed, {"type": "undo", "path": "./u.txt"})
    assert (tmp_path / "sd" / "u.txt").read_text() == "two\n"


async def test_undo_of_a_created_file_deletes_it(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "new.txt", "x\n")
    r = await _call(seeded_kernel, ed, {"type": "undo", "path": "new.txt"})
    assert r == {"undone": True, "path": "new.txt", "deleted": True}, r
    assert not (tmp_path / "sd" / "new.txt").exists()


async def test_undo_reverts_an_edit_and_an_append(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "e.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "e.py", _block("a = 1", "a = 2"))
    assert r["edited"] is True, r
    await _call(seeded_kernel, ed, {"type": "undo", "path": "e.py"})
    assert (tmp_path / "sd" / "e.py").read_text() == "a = 1\n"
    await _call(
        seeded_kernel, ed, {"type": "append", "path": "e.py", "bytes": b"b = 2\n"}
    )
    await _call(seeded_kernel, ed, {"type": "undo", "path": "e.py"})
    assert (tmp_path / "sd" / "e.py").read_text() == "a = 1\n"


async def test_undo_with_no_slot_is_not_found(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    (tmp_path / "sd").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / "z.txt").write_text("z\n")
    r = await _call(seeded_kernel, ed, {"type": "undo", "path": "z.txt"})
    assert r["reason"] == "not_found" and "nothing to undo" in r["error"], r


async def test_undo_is_refused_on_a_readonly_leg(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel, fb_meta={"readonly": True})
    r = await _call(seeded_kernel, ed, {"type": "undo", "path": "z.txt"})
    assert r["reason"] == "unauthorized", r


async def test_text_verbs_see_only_exposed_paths(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel, fb_meta={"allow": ["pub"]})
    (tmp_path / "sd" / "pub").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / ".env").write_text("SECRET=1\n")
    for verb in ("read", "write", "append", "undo"):
        r = await _call(
            seeded_kernel, ed, {"type": verb, "path": ".env", "bytes": b"x"}
        )
        assert r["reason"] == "not_found", (verb, r)
    assert (tmp_path / "sd" / ".env").read_text() == "SECRET=1\n"
    r = await _call(
        seeded_kernel, ed, {"type": "write", "path": "pub/a.txt", "bytes": b"ok"}
    )
    assert r["size"] == 2, r
