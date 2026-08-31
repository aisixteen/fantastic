"""file_bridge.patch_text_file — SEARCH/REPLACE whole-file edit over the stream.

The patch rides as raw utf-8 BYTES (the binary channel), so a hunk full of quotes,
backslashes, and newlines is applied VERBATIM — never JSON-escaped. Each SEARCH must
match the current content EXACTLY ONCE; not-found / ambiguous FAILS LOUD and writes
nothing (all-or-nothing, no fuzzy apply).
"""

from __future__ import annotations


async def _open(kernel, **meta):
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "ingress_rule": "allow_all_ingress",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    return r["id"]


async def _seed(kernel, fid, path, text):
    return (
        await kernel.send(
            [
                {
                    "target": fid,
                    "payload": {
                        "type": "write_stream",
                        "path": path,
                        "bytes": text.encode("utf-8"),
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]


async def _patch(kernel, fid, path, patch, **extra):
    payload = {"type": "patch_text_file", "path": path, **extra}
    if patch is not None:
        payload["bytes"] = patch.encode("utf-8") if isinstance(patch, str) else patch
    return (await kernel.send([{"target": fid, "payload": payload}]))[0]["result"]


def _block(old, new):
    return f"<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE"


# ─── happy paths ────────────────────────────────────────────────


async def test_single_hunk_applies_and_writes(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\nb = 2\nc = 3\n")
    r = await _patch(seeded_kernel, fid, "m.py", _block("b = 2", "b = 20"))
    assert r["patched"] is True
    assert r["hunks"] == 1
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 20\nc = 3\n"


async def test_multiple_hunks_apply_in_order(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "x = 1\ny = 2\nz = 3\n")
    patch = _block("x = 1", "x = 11") + "\n" + _block("z = 3", "z = 33")
    r = await _patch(seeded_kernel, fid, "m.py", patch)
    assert r["patched"] is True and r["hunks"] == 2
    assert (tmp_path / "sd" / "m.py").read_text() == "x = 11\ny = 2\nz = 33\n"


async def test_hunk_matches_content_a_prior_hunk_created(
    seeded_kernel, tmp_path, monkeypatch
):
    # Hunks apply against the EVOLVING content: hunk 2's SEARCH exists only after hunk 1.
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.txt", "one\n")
    patch = _block("one", "two") + "\n" + _block("two", "three")
    r = await _patch(seeded_kernel, fid, "m.txt", patch)
    assert r["patched"] is True and r["hunks"] == 2
    assert (tmp_path / "sd" / "m.txt").read_text() == "three\n"


async def test_hunk_with_quotes_backslashes_newlines_is_verbatim(
    seeded_kernel, tmp_path, monkeypatch
):
    # The whole point of bytes-on-the-wire: no escaping. A quote-heavy program applies
    # exactly as written.
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    original = 'print("hi")\npath = "c:\\\\tmp"\n'
    await _seed(seeded_kernel, fid, "p.py", original)
    new = 'print("bye", end="")\npath = "c:\\\\tmp"\n'
    r = await _patch(
        seeded_kernel, fid, "p.py", _block('print("hi")', 'print("bye", end="")')
    )
    assert r["patched"] is True
    assert (tmp_path / "sd" / "p.py").read_text() == new


async def test_unicode_content_and_hunk(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "u.txt", "café — over\nnext\n")
    r = await _patch(seeded_kernel, fid, "u.txt", _block("café — over", "tea — done"))
    assert r["patched"] is True
    assert (tmp_path / "sd" / "u.txt").read_text() == "tea — done\nnext\n"


async def test_multiline_hunk(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(
        seeded_kernel, fid, "m.py", "def f():\n    return 1\n\ndef g():\n    return 2\n"
    )
    r = await _patch(
        seeded_kernel,
        fid,
        "m.py",
        _block("def f():\n    return 1", "def f():\n    return 99"),
    )
    assert r["patched"] is True
    assert (tmp_path / "sd" / "m.py").read_text() == (
        "def f():\n    return 99\n\ndef g():\n    return 2\n"
    )


# ─── fail-loud, all-or-nothing ──────────────────────────────────


async def test_no_match_fails_and_writes_nothing(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", _block("nope = 0", "x = 0"))
    assert "patched" not in r
    assert r["reason"] == "no_match"
    assert r["failed"][0]["index"] == 0
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"  # untouched


async def test_ambiguous_match_fails_and_writes_nothing(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "v = 1\nv = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", _block("v = 1", "v = 2"))
    assert "patched" not in r
    assert r["reason"] == "ambiguous"
    assert "2 places" in r["failed"][0]["reason"]
    assert (tmp_path / "sd" / "m.py").read_text() == "v = 1\nv = 1\n"  # untouched


async def test_second_hunk_failure_reverts_the_whole_patch(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\nb = 2\n")
    patch = _block("a = 1", "a = 10") + "\n" + _block("missing", "x")
    r = await _patch(seeded_kernel, fid, "m.py", patch)
    assert "patched" not in r and r["failed"][0]["index"] == 1
    # first hunk WAS valid but nothing is written — all-or-nothing.
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\nb = 2\n"


# ─── bad input ──────────────────────────────────────────────────


async def test_malformed_patch_unterminated_block(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    bad_patch = "<<<<<<< SEARCH\na = 1\n=======\na = 2\n"  # no >>>>>>> REPLACE
    r = await _patch(seeded_kernel, fid, "m.py", bad_patch)
    assert "patched" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_empty_patch_no_blocks(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", "just some prose, no markers")
    assert "patched" not in r and r["reason"] == "bad_arg"


async def test_missing_bytes(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", None)
    assert "patched" not in r and "bytes" in r["error"]


async def test_file_not_found(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    r = await _patch(seeded_kernel, fid, "nope.py", _block("a", "b"))
    assert "patched" not in r and r["reason"] == "not_found"


async def test_missing_path(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": fid,
                    "payload": {
                        "type": "patch_text_file",
                        "bytes": _block("a", "b").encode("utf-8"),
                    },
                }
            ]
        )
    )[0]["result"]
    assert "patched" not in r and "path" in r["error"]


async def test_bad_utf8_patch(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", b"\xff\xfe\xff")  # not utf-8
    assert "patched" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_malformed_missing_divider(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    await _seed(seeded_kernel, fid, "m.py", "a = 1\n")
    # a SEARCH block that never reaches `=======` before it ends
    bad_patch = "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE"
    r = await _patch(seeded_kernel, fid, "m.py", bad_patch)
    assert "patched" not in r and r["reason"] == "bad_arg"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


# ─── gate + posture ─────────────────────────────────────────────


async def test_readonly_refuses(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd", readonly=True)
    (tmp_path / "sd").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / "m.py").write_text("a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", _block("a = 1", "a = 2"))
    assert r["reason"] == "unauthorized"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"


async def test_sealed_bridge_denies_patch(seeded_kernel, tmp_path, monkeypatch):
    # No ingress_rule ⇒ sealed by default; every verb but reflect denies.
    monkeypatch.chdir(tmp_path)
    r0 = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "root": "sd",
                    },
                }
            ]
        )
    )[0]["result"]
    fid = r0["id"]
    (tmp_path / "sd").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sd" / "m.py").write_text("a = 1\n")
    r = await _patch(seeded_kernel, fid, "m.py", _block("a = 1", "a = 2"))
    assert r["reason"] == "unauthorized"
    assert (tmp_path / "sd" / "m.py").read_text() == "a = 1\n"
