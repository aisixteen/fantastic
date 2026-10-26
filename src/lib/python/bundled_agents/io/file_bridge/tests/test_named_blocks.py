"""file_bridge's `edit` with NAMED blocks (#206): a block may name its file on
its opening line (`<<<<<<< SEARCH path`), so a multi-file edit needs no hunks counts.
Port of Rust #231 — same names, same assertions."""

from __future__ import annotations

import pathlib


async def _mk(kernel, *, fb_meta=None):
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


async def _seed(kernel, ed, path, text):
    return await _call(
        kernel, ed, {"type": "write", "path": path, "bytes": text.encode("utf-8")}
    )


def _block(old, new):
    return f"<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE"


def _named(path, old, new):
    return f"<<<<<<< SEARCH {path}\n{old}\n=======\n{new}\n>>>>>>> REPLACE"


async def _edit_files(kernel, ed, paths, patch):
    return await _call(
        kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": p} for p in paths],
            "bytes": patch.encode("utf-8"),
        },
    )


async def test_named_blocks_split_a_two_file_edit_without_hunks(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "one\n")
    await _seed(seeded_kernel, ed, "b.txt", "two\n")
    patch = _named("a.txt", "one", "uno") + "\n" + _named("b.txt", "two", "dos")
    r = await _edit_files(seeded_kernel, ed, ["a.txt", "b.txt"], patch)
    assert r["edited"] is True
    assert [e["hunks"] for e in r["edits"]] == [1, 1]
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "uno\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "dos\n"


async def test_a_block_naming_a_file_outside_edits_is_refused(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "one\n")
    await _seed(seeded_kernel, ed, "b.txt", "two\n")
    patch = _named("a.txt", "one", "uno") + "\n" + _named("c.txt", "two", "dos")
    r = await _edit_files(seeded_kernel, ed, ["a.txt", "b.txt"], patch)
    assert "edited" not in r
    assert r["reason"] == "bad_arg"
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "one\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "two\n"


async def test_one_file_without_hunks_applies_every_block(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "m.txt", "x\ny\n")
    patch = _block("x", "X") + "\n" + _block("y", "Y")
    r = await _edit_files(seeded_kernel, ed, ["m.txt"], patch)
    assert r["edited"] is True and r["edits"][0]["hunks"] == 2
    assert (pathlib.Path(tmp_path) / "sd" / "m.txt").read_text(
        encoding="utf-8"
    ) == "X\nY\n"


async def test_a_failure_in_the_second_file_writes_nothing(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "one\n")
    await _seed(seeded_kernel, ed, "b.txt", "two\n")
    patch = _named("a.txt", "one", "uno") + "\n" + _named("b.txt", "absent", "x")
    r = await _edit_files(seeded_kernel, ed, ["a.txt", "b.txt"], patch)
    assert "edited" not in r
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "one\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "two\n"


def test_edit_grammar_text_matches_rust():
    import file_bridge.tools as tools

    src = pathlib.Path(tools.__file__).read_text(encoding="utf-8")
    assert (
        "with several files, a block may name its file on its first line "
        "(`<<<<<<< SEARCH path`), and then no hunks are needed; any failure anywhere "
    ) in src


# ─── Rust twins (src/lib/rust/crates/bundles/fantastic-file/src/tests_text.rs) ─


async def test_named_blocks_may_come_in_any_order(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "x = 1\nw = 3\n")
    await _seed(seeded_kernel, ed, "b.txt", "y = 1\n")
    patch = (
        _named("a.txt", "x = 1", "x = 2")
        + "\n"
        + _named("b.txt", "y = 1", "y = 2")
        + "\n"
        + _named("a.txt", "w = 3", "w = 4")
    )
    r = await _edit_files(seeded_kernel, ed, ["a.txt", "b.txt"], patch)
    assert r["edited"] is True, r
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "x = 2\nw = 4\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "y = 2\n"


async def test_named_and_unnamed_blocks_do_not_mix(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "x = 1\n")
    await _seed(seeded_kernel, ed, "b.txt", "y = 1\n")
    patch = _named("a.txt", "x = 1", "x = 2") + "\n" + _block("y = 1", "y = 2")
    r = await _edit_files(seeded_kernel, ed, ["a.txt", "b.txt"], patch)
    assert r["reason"] == "bad_arg" and r["arg"] == "bytes", r
    assert "every block or on none" in r["error"], r
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "x = 1\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "y = 1\n"


async def test_a_stated_hunks_must_match_the_named_blocks(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fb, ed = await _mk(seeded_kernel)
    await _seed(seeded_kernel, ed, "a.txt", "x = 1\nw = 3\n")
    await _seed(seeded_kernel, ed, "b.txt", "y = 1\n")
    patch = (
        _named("a.txt", "x = 1", "x = 2")
        + "\n"
        + _named("a.txt", "w = 3", "w = 4")
        + "\n"
        + _named("b.txt", "y = 1", "y = 2")
    )
    r = await _call(
        seeded_kernel,
        ed,
        {
            "type": "edit",
            "edits": [{"path": "a.txt", "hunks": 1}, {"path": "b.txt"}],
            "bytes": patch.encode("utf-8"),
        },
    )
    assert r["reason"] == "bad_arg", r
    assert "leave hunks out" in r["error"], r
    root = pathlib.Path(tmp_path) / "sd"
    assert (root / "a.txt").read_text(encoding="utf-8") == "x = 1\nw = 3\n"
    assert (root / "b.txt").read_text(encoding="utf-8") == "y = 1\n"
