"""Issue #305: a refused or not-found path names the root and the relative form.

Mirrors the Rust tests in fantastic-file (tests_paths.rs)."""

from __future__ import annotations

from file_bridge import fs


def _root(tmp_path):
    """A root named `wt-x` under the tmp dir: returns (Root, realpath of the root)."""
    (tmp_path / "wt-x").mkdir()
    R = fs.Root(tmp_path, "wt-x")
    return R, fs.resolve_root(R)


def test_absolute_path_inside_root_refusal_names_relative_form(tmp_path):
    R, root_p = _root(tmp_path)
    abs_path = str(root_p / "a.rs")
    try:
        fs.resolve(R, abs_path)
    except ValueError as e:
        err = str(e)
    else:
        raise AssertionError("an absolute path must be refused")
    assert "escapes root" in err, err
    assert f"paths are relative to the root {root_p}" in err, err
    assert err.endswith("e.g. a.rs"), err


def test_redundant_leading_root_segment_not_found_suggests_stripped_path(tmp_path):
    _, root_p = _root(tmp_path)
    hint = fs.not_found_hint(root_p, "wt-x/a.rs")
    assert f"paths are relative to the root {root_p}" in hint, hint
    assert hint.endswith("e.g. a.rs"), hint


async def test_read_of_a_redundant_root_path_names_the_hint(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "wt-x").mkdir()
    fb = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "wt-x",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    r = (
        await seeded_kernel.send(
            [{"target": fb, "payload": {"type": "read", "path": "wt-x/a.rs"}}]
        )
    )[0]["result"]
    assert r["reason"] == "not_found", r
    assert "paths are relative to the root" in r["error"], r
    assert r["error"].endswith("e.g. a.rs"), r


def test_outside_path_refusal_names_root_without_suggestion(tmp_path):
    R, root_p = _root(tmp_path)
    try:
        fs.resolve(R, "../escape")
    except ValueError as e:
        err = str(e)
    else:
        raise AssertionError("an escaping path must be refused")
    assert f"paths are relative to the root {root_p}" in err, err
    assert "e.g." not in err, err
