"""META BUNDLES — a `bundles/<name>/bundle.yaml` with a `base` and no program: the
yaml over a compiled bundle, in RAM. An instance's `handler_module` is the meta
bundle; it runs the base's code with the bundle's `record` under its own fields.
The twin of rust `fantastic-core/src/tests_meta_bundles.rs`. The base here is the
file bridge: any registered bundle will do."""

from __future__ import annotations

from pathlib import Path

import pytest
from file_bridge import fs
from kernel import _bundles
from kernel_state.tools import load_bundles

NOTES = (
    "base: file_bridge.tools\nsentence: A notes folder.\nrecord:\n  root: notes\n"
    "  allow: ['.']\n  ingress_rule: allow_all_ingress\n"
)


@pytest.fixture(autouse=True)
def _forget_folders():
    """The folder registry is process-wide: drop what a test loaded."""
    yield
    _bundles._FOLDER.clear()
    _bundles._FOLDER_ROWS.clear()


def _bundle(kernel, name: str, yaml: str) -> None:
    folder = Path(kernel.ctx.workdir) / ".fantastic" / "bundles" / name
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "bundle.yaml").write_text(yaml)


def _load(kernel, bundles: dict[str, str]) -> None:
    for name, yaml in bundles.items():
        _bundle(kernel, name, yaml)
    load_bundles(fs.Root(kernel.ctx.workdir, ".fantastic"))


async def _call(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _create(kernel, **fields):
    return await _call(
        kernel, kernel.id, {"type": "create_agent", "parent_id": kernel.id, **fields}
    )


def _meta(kernel, id_: str) -> dict:
    return dict(kernel.ctx.agents[id_]._meta)


async def test_an_instance_of_a_meta_bundle_inherits_its_fields_and_runs_its_base(kernel):
    notes = Path(kernel.ctx.workdir) / "notes"
    notes.mkdir()
    (notes / "a.md").write_text("hello\n")
    _load(kernel, {"notes": NOTES})
    r = await _create(kernel, id="n1", handler_module="notes")
    assert r.get("id") == "n1", r
    m = _meta(kernel, "n1")
    assert m["root"] == "notes" and m["allow"] == ["."]
    assert kernel.ctx.agents["n1"].handler_module == "notes", "the kind stays the meta bundle"
    read = await _call(kernel, "n1", {"type": "read", "path": "a.md"})
    assert read["lines"][0]["text"] == "hello", read
    row = next(r for r in _bundles.folder_rows() if r["name"] == "notes")
    assert row["base"] == "file_bridge.tools" and "record" not in row, row


async def test_a_field_the_create_names_overrides_the_inherited_one(kernel):
    _load(kernel, {"notes": NOTES})
    await _create(kernel, id="n2", handler_module="notes", root="elsewhere")
    m = _meta(kernel, "n2")
    assert m["root"] == "elsewhere", "the instance's own value wins"
    assert m["allow"] == ["."], "the rest is inherited"


async def test_bundle_beside_the_base_is_the_same_create(kernel):
    _load(kernel, {"notes": NOTES})
    r = await _create(kernel, id="n3", handler_module="file_bridge.tools", bundle="notes")
    assert r.get("id") == "n3", r
    a = kernel.ctx.agents["n3"]
    assert a.handler_module == "notes"
    assert "bundle" not in a._meta, "the record carries the kind, not a `bundle` field"
    assert a._meta["root"] == "notes"


async def test_a_meta_bundle_extends_a_meta_bundle_the_nearest_field_wins(kernel):
    # `drafts` sorts before `notes`: the boot load takes it on a second pass
    drafts = "base: notes\nsentence: Drafts.\nrecord:\n  root: drafts\n"
    _load(kernel, {"notes": NOTES, "drafts": drafts})
    await _create(kernel, id="d1", handler_module="drafts")
    m = _meta(kernel, "d1")
    assert m["root"] == "drafts", "the nearer bundle wins"
    assert m["ingress_rule"] == "allow_all_ingress", "the farther one fills the rest"
    assert _bundles.meta_chain("drafts") == ["drafts", "notes"]


async def test_a_reload_rewrites_the_inherited_fields_and_keeps_an_instances_own(kernel):
    _load(kernel, {"notes": NOTES})
    await _create(kernel, id="i1", handler_module="notes")
    await _create(kernel, id="i2", handler_module="notes", root="mine")
    _bundle(
        kernel,
        "notes",
        "base: file_bridge.tools\nsentence: A notes folder.\nrecord:\n  root: notes2\n  allow: ['.']\n",
    )
    r = await _call(kernel, kernel.id, {"type": "reload_bundle", "name": "notes"})
    assert r["reloaded"] == "notes", r
    m1, m2 = _meta(kernel, "i1"), _meta(kernel, "i2")
    assert m1["root"] == "notes2", "an inherited field takes the new value"
    assert "ingress_rule" not in m1, "a field the bundle dropped goes"
    assert m2["root"] == "mine", "the instance's own field stays"
    assert r["instances"] == ["i1", "i2"], r


async def test_a_meta_bundle_is_refused_naming_the_field(kernel):
    _load(
        kernel,
        {
            "nobase": "base: no_such.tools\nsentence: s\n",
            "badrec": "base: file_bridge.tools\nsentence: s\nrecord: [1]\n",
            "loop_a": "base: loop_b\nsentence: s\n",
            "loop_b": "base: loop_a\nsentence: s\n",
        },
    )
    rows = {r["name"]: r for r in _bundles.folder_rows()}
    assert "base: 'no_such.tools' is no bundle" in rows["nobase"]["error"], rows["nobase"]
    assert "record: must be a mapping" in rows["badrec"]["error"], rows["badrec"]
    assert "base:" in rows["loop_a"]["error"] and "base:" in rows["loop_b"]["error"], rows
    assert _bundles.meta_chain("nobase") == []


async def test_a_record_loaded_from_disk_inherits_like_a_create(kernel):
    _load(kernel, {"notes": NOTES})
    root = dict(kernel.record)
    kernel.ctx.load(
        [root, {"id": "n9", "handler_module": "notes", "parent_id": root["id"], "root": "mine"}]
    )
    m = dict(kernel.ctx.agents["n9"]._meta)
    assert m["root"] == "mine", "the record's own field wins"
    assert m["allow"] == ["."], "the bundle's fills the rest"


async def test_a_bundle_on_another_base_keeps_that_handler_and_names_the_bundle(kernel):
    import file_bridge.tools as fb

    _bundles.register_unlisted("files_two.tools", fb)
    try:
        _load(kernel, {"notes": NOTES})
        r = await _create(kernel, id="o1", handler_module="files_two.tools", bundle="notes")
        assert r.get("id") == "o1", r
        a = kernel.ctx.agents["o1"]
        assert a.handler_module == "files_two.tools", "it runs the base it named"
        assert a._meta["bundle"] == "notes", "and names the bundle it inherits from"
        assert a._meta["root"] == "notes"
        _bundle(kernel, "notes", "base: file_bridge.tools\nsentence: s\nrecord:\n  root: notes3\n")
        r = await _call(kernel, kernel.id, {"type": "reload_bundle", "name": "notes"})
        assert r["instances"] == ["o1"], r
        assert a._meta["root"] == "notes3", "a reload reaches it too"
    finally:
        _bundles._UNLISTED.pop("files_two.tools", None)
