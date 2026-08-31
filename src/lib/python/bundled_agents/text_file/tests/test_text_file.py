"""text_file — the stateless text-document agent, every byte a GATED send.

The patch cases are the file_bridge `patch_text_file` suite, ported to the
send-path: the editor never touches disk; it sends stream verbs to the
file_bridge named by `on`/`target_id`. The old readonly/sealed posture cases
become "the wired LEG refuses" — the leg's own gate answers, and the editor
passes the refusal through verbatim.
"""

from __future__ import annotations

from pathlib import Path


async def _mk(kernel, *, fb_meta=None, editor_meta=None):
    """One open file_bridge + one editor wired to it (both gate ends granted)."""
    fb = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "root": "sd",
                        "ingress_rule": "allow_all_ingress",
                        **(fb_meta or {}),
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    ed = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "text_file.tools",
                        "target_id": fb,
                        "can_call": {fb: "*"},
                        **(editor_meta or {}),
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    await kernel.send(
        [
            {
                "target": "kernel_state",
                "payload": {
                    "type": "update_agent",
                    "id": fb,
                    "callable_by": {ed: "*"},
                },
            }
        ]
    )
    return fb, ed


async def _call(kernel, ed, payload):
    return (await kernel.send([{"target": ed, "payload": payload}]))[0]["result"]


async def _patch(kernel, ed, path, patch, *, hunks=None, if_match=None, **extra):
    """One-file patch through the TRANSACTION shape: even a single file rides
    the `edits` array (one shape, no drift)."""
    edit = {
        "path": path,
        "hunks": hunks
        if hunks is not None
        else max(1, str(patch).count("<<<<<<< SEARCH")),
    }
    if if_match is not None:
        edit["if_match"] = if_match
    payload = {"type": "patch", "edits": [edit], **extra}
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
    assert r["patched"] is True and r["edits"][0]["hunks"] == 1
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 20\nc = 3\n"


async def test_multiple_hunks_apply_in_order(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "x = 1\ny = 2\nz = 3\n")
    patch = _block("x = 1", "x = 11") + "\n" + _block("z = 3", "z = 33")
    r = await _patch(seeded_kernel, ed, "m.py", patch)
    assert r["patched"] is True and r["edits"][0]["hunks"] == 2
    assert (tmp_path / "sd" / "m.py").read_text() == "x = 11\ny = 2\nz = 33\n"


async def test_hunk_matches_content_a_prior_hunk_created(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.txt", "one\n")
    patch = _block("one", "two") + "\n" + _block("two", "three")
    r = await _patch(seeded_kernel, ed, "m.txt", patch)
    assert r["patched"] is True and r["edits"][0]["hunks"] == 2
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
    assert r["patched"] is True
    assert (tmp_path / "sd" / "p.py").read_text() == new


async def test_unicode_content_and_hunk(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "u.txt", "café — over\nnext\n")
    r = await _patch(seeded_kernel, ed, "u.txt", _block("café — over", "tea — done"))
    assert r["patched"] is True
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
    assert r["patched"] is True
    assert (tmp_path / "sd" / "m.py").read_text() == (
        "def f():\n    return 99\n\ndef g():\n    return 2\n"
    )


# ─── patch: fail-loud, all-or-nothing ───────────────────────────


async def test_no_match_fails_and_writes_nothing(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", _block("nope = 0", "x = 0"))
    assert "patched" not in r
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
    assert "patched" not in r
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
    assert "patched" not in r and r["failed"][0]["index"] == 1
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 2\n"


# ─── patch: bad input ───────────────────────────────────────────


async def test_malformed_patch_unterminated_block(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    bad_patch = "<<<<<<< SEARCH\na = 1\n=======\na = 2\n"  # no >>>>>>> REPLACE
    r = await _patch(seeded_kernel, ed, "m.py", bad_patch)
    assert "patched" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_empty_patch_no_blocks(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", "just some prose, no markers")
    assert "patched" not in r and r["reason"] == "bad_arg"


async def test_missing_bytes(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", None)
    assert "patched" not in r and "bytes" in r["error"]


async def test_file_not_found(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _patch(seeded_kernel, ed, "nope.py", _block("a", "b"))
    assert "patched" not in r and r["reason"] == "not_found"


async def test_missing_path(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    r = await _call(
        seeded_kernel, ed, {"type": "patch", "bytes": _block("a", "b").encode("utf-8")}
    )
    assert "patched" not in r and "edits" in r["error"]


async def test_bad_utf8_patch(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, ed, "m.py", b"\xff\xfe\xff")
    assert "patched" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_malformed_missing_divider(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.py", "a = 1\n")
    bad_patch = "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE"
    r = await _patch(seeded_kernel, ed, "m.py", bad_patch)
    assert "patched" not in r and r["reason"] == "bad_arg"
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


async def test_checkpointed_write_restores_byte_identical(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "c.txt", "original\n")
    w = await _call(
        seeded_kernel,
        ed,
        {"type": "write", "path": "c.txt", "bytes": b"mutated\n", "checkpoint": True},
    )
    ck = w["ckpt_id"]
    assert ":" in ck  # the dispatch's kernel task pid
    assert (tmp_path / "sd" / "c.txt").read_text() == "mutated\n"
    r = await _call(
        seeded_kernel, ed, {"type": "restore", "path": "c.txt", "ckpt_id": ck}
    )
    assert r["restored"] is True
    assert (tmp_path / "sd" / "c.txt").read_text() == "original\n"


async def test_checkpoint_of_absent_file_restores_to_absence(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    w = await _call(
        seeded_kernel,
        ed,
        {"type": "write", "path": "new.txt", "bytes": b"born\n", "checkpoint": True},
    )
    assert (tmp_path / "sd" / "new.txt").exists()
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "restore", "path": "new.txt", "ckpt_id": w["ckpt_id"]},
    )
    assert r["restored"] is True and r["deleted"] is True
    assert not (tmp_path / "sd" / "new.txt").exists()


async def test_explicit_checkpoint_then_patch_then_restore(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "p.txt", "a = 1\n")
    c = await _call(seeded_kernel, ed, {"type": "checkpoint", "path": "p.txt"})
    await _patch(seeded_kernel, ed, "p.txt", _block("a = 1", "a = 2"))
    ls = await _call(seeded_kernel, ed, {"type": "checkpoints", "path": "p.txt"})
    assert [x["id"] for x in ls["ckpts"]] == [c["ckpt_id"]]
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "restore", "path": "p.txt", "ckpt_id": c["ckpt_id"]},
    )
    assert r["version"] == c["version"]
    assert (tmp_path / "sd" / "p.txt").read_text() == "a = 1\n"


async def test_drop_discards_the_preimage(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "x.txt", "keep\n")
    c = await _call(seeded_kernel, ed, {"type": "checkpoint", "path": "x.txt"})
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "drop", "path": "x.txt", "ckpt_id": c["ckpt_id"]},
    )
    assert r["dropped"] is True
    ls = await _call(seeded_kernel, ed, {"type": "checkpoints", "path": "x.txt"})
    assert ls["ckpts"] == []
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "restore", "path": "x.txt", "ckpt_id": c["ckpt_id"]},
    )
    assert r["reason"] == "not_found"


async def test_restore_unknown_ckpt_fails_loud(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "y.txt", "v\n")
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "restore", "path": "y.txt", "ckpt_id": "tf:deadbeef"},
    )
    assert r["reason"] == "not_found"
    assert (tmp_path / "sd" / "y.txt").read_text() == "v\n"


# ─── unwired failfast + `on` override ───────────────────────────


async def test_unwired_editor_fails_loud_on_every_verb(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    ed = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "text_file.tools",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    for verb in ("read", "write", "patch", "append", "checkpoint", "restore", "drop"):
        r = await _call(seeded_kernel, ed, {"type": verb, "path": "x"})
        assert r["reason"] == "not_found" and "target" in r["error"], (verb, r)


async def test_on_overrides_the_record_target(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    fb2 = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "root": "other",
                        "ingress_rule": "allow_all_ingress",
                        "callable_by": {ed: "*"},
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    await seeded_kernel.send(
        [
            {
                "target": "kernel_state",
                "payload": {
                    "type": "update_agent",
                    "id": ed,
                    "can_call": {fb2: "*"},
                },
            }
        ]
    )
    r = await _call(
        seeded_kernel,
        ed,
        {"type": "write", "path": "z.txt", "bytes": b"here\n", "on": fb2},
    )
    assert not r.get("error"), r
    assert (tmp_path / "other" / "z.txt").read_text() == "here\n"
    assert not (tmp_path / "sd" / "z.txt").exists()


# ─── the no-bypass proof ────────────────────────────────────────


def test_package_has_no_disk_code():
    """The grep-proof: the bundle contains no fs import and no open() — every
    byte must be a send to the leg. This is the clamp's unbypassability, stated
    as a test."""
    pkg = Path(__file__).parent.parent / "src" / "text_file"
    for py in pkg.rglob("*.py"):
        src = py.read_text()
        assert "open(" not in src, f"{py.name} calls open()"
        for line in src.splitlines():
            if line.strip().startswith(("import ", "from ")):
                assert "fs" not in line.split("#")[0], f"{py.name}: {line!r}"


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
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 2}],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert r["patched"] is True, r
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
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert "patched" not in r and r["reason"] == "no_match", r
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
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 2}],
            "bytes": _block("x = 1", "x = 2").encode("utf-8"),
        },
    )
    assert r["reason"] == "bad_arg" and "sum" in r["error"], r
    assert (tmp_path / "sd" / "a.py").read_text() == "x = 1\n"


async def test_multifile_checkpoint_true_keeps_preimages(
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
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 1}, {"path": "b.py", "hunks": 1}],
            "bytes": blob.encode("utf-8"),
            "checkpoint": True,
        },
    )
    assert r["patched"] is True and len(r["ckpt_ids"]) == 2, r
    for row in r["ckpt_ids"]:
        rr = await _call(
            seeded_kernel,
            ed,
            {"type": "restore", "path": row["path"], "ckpt_id": row["ckpt_id"]},
        )
        assert rr["restored"] is True, rr
    assert (tmp_path / "sd" / "a.py").read_text() == "one\n"
    assert (tmp_path / "sd" / "b.py").read_text() == "two\n"


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
            "type": "patch",
            "edits": [
                {"path": "a.py", "hunks": 1},
                {"path": "b.py", "hunks": 1, "if_match": "0" * 16},
            ],
            "bytes": blob.encode("utf-8"),
        },
    )
    assert r["reason"] == "version_mismatch" and r["path"] == "b.py", r
    assert (tmp_path / "sd" / "a.py").read_text() == "one\n"
