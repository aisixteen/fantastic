"""A record's `handler_module` resolves ONLY from the `fantastic.bundles` catalog
plus `register_unlisted` — never from all of `sys.path` (audit 2026-10-03, ARCH2-1).

Importing any name on `sys.path` ran that module's top-level code on a grant-less
`create_agent` under the sender itself. A name outside the catalog is now, to this
kernel, a module that does not exist — rust's and swift's answer for an
unregistered name, in their text.
"""

from __future__ import annotations

import types

from kernel import Kernel, register_unlisted, sender_context
from kernel._bundles import import_handler


def _plant(tmp_path, monkeypatch, name: str):
    """A real module on `sys.path` whose top level leaves a marker file when it
    runs — the import side effect the catalog keeps from ever happening."""
    marker = tmp_path / f"{name}.ran"
    (tmp_path / f"{name}.py").write_text(
        f"open({str(marker)!r}, 'w').write('ran')\n"
        "async def handler(id, payload, kernel):\n"
        "    return {'answered': id}\n"
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    return marker


async def _one(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]


async def test_a_module_on_sys_path_outside_the_catalog_never_runs(
    seeded_kernel, tmp_path, monkeypatch
):
    marker = _plant(tmp_path, monkeypatch, "arch21_sidefx")
    seeded_kernel.create(None, id="arch21_maker")  # the structural baseline only
    with sender_context("arch21_maker"):
        made = await _one(
            seeded_kernel,
            "kernel",
            {
                "type": "create_agent",
                "parent_id": "arch21_maker",
                "handler_module": "arch21_sidefx",
                "id": "arch21_rec",
            },
        )
    # kept as a record this runtime cannot answer — as rust and swift keep it
    assert made["status"] == "completed", made
    called = await _one(seeded_kernel, "arch21_rec", {"type": "ping"})
    assert called["result"] == {
        "error": (
            "agent 'arch21_rec': handler_module 'arch21_sidefx' cannot be imported "
            "in this runtime: ModuleNotFoundError: No module named 'arch21_sidefx'"
        ),
        "reason": "unhandled",
        "verb": "ping",
    }
    r = await _one(seeded_kernel, "kernel", {"type": "reflect", "detail": "full"})
    assert r["status"] == "completed", r
    assert not marker.exists(), "the module's top level ran"


def test_the_refusal_names_the_top_level_package_as_rust_does():
    """`unittest.__main__` exits the process at import; `file_bridge.nope` sits
    under an installed package. Neither is imported, and both name their
    top-level package — rust's `no_module_named`."""
    for name, top in (
        ("unittest.__main__", "unittest"),
        ("file_bridge.nope", "file_bridge"),
    ):
        try:
            import_handler(name)
        except ModuleNotFoundError as e:
            assert str(e) == f"No module named {top!r}", e
        else:
            raise AssertionError(f"{name} resolved")


def test_load_skips_a_record_outside_the_catalog_without_importing_it(
    tmp_path, monkeypatch, capsys
):
    marker = _plant(tmp_path, monkeypatch, "arch21_load")
    k = Kernel()
    k.load(
        [
            {"id": "kernel_state", "handler_module": "kernel_state.tools"},
            {
                "id": "ghost",
                "parent_id": "kernel_state",
                "handler_module": "arch21_load",
            },
        ],
        root_path=tmp_path / ".fantastic",
    )
    assert "ghost" not in k.agents
    assert (
        "[kernel] skipping agent ghost: bundle arch21_load not installed in this runtime"
        in capsys.readouterr().err
    )
    assert not marker.exists()


async def test_an_unlisted_module_dispatches_reloads_and_stays_out_of_the_catalog(
    seeded_kernel,
):
    """`register_unlisted` — rust's `BundleRegistry::register_unlisted`: the module
    answers (dispatch AND `Kernel.load`), and `reflect bundles` never lists it."""
    mod = types.ModuleType("arch21_unlisted.tools")

    async def handler(id, payload, kernel):
        return {"answered": id}

    mod.handler = handler
    mod.VERBS = {}
    register_unlisted("arch21_unlisted.tools", mod)
    seeded_kernel.create("arch21_unlisted.tools", id="ul")
    assert (await _one(seeded_kernel, "ul", {"type": "ping"}))["result"] == {
        "answered": "ul"
    }
    r = await _one(seeded_kernel, "kernel", {"type": "reflect", "bundles": "all"})
    listed = {b["handler_module"] for b in r["result"]["bundles"]}
    assert "file_bridge.tools" in listed and "arch21_unlisted.tools" not in listed
    k2 = Kernel()
    k2.load(seeded_kernel.ctx.save())
    assert k2.agents["ul"].handler_module == "arch21_unlisted.tools"


async def test_persisting_a_non_catalog_record_imports_nothing(
    kernel, tmp_path, monkeypatch
):
    """The loader's readme seed resolves the package through the catalog too:
    reading a package's resources imports its `__init__`, so a record naming a
    package outside the catalog seeds nothing and runs nothing (ARCH2-1)."""
    import sys
    from pathlib import Path

    from _testkit import persist

    marker = tmp_path / "arch21_pkgfx.ran"
    pkg = tmp_path / "arch21_pkgfx"
    pkg.mkdir()
    (pkg / "__init__.py").write_text(f"open({str(marker)!r}, 'w').write('ran')\n")
    (pkg / "readme.md").write_text("PLANTED")
    monkeypatch.syspath_prepend(str(tmp_path))
    kernel.create("arch21_pkgfx.tools", id="arch21_pkg_rec")
    persist(kernel)
    assert not marker.exists(), "the package's __init__ ran"
    assert "arch21_pkgfx" not in sys.modules
    root = Path(kernel.ctx.agents["arch21_pkg_rec"]._root_path)
    assert not (root / "readme.md").exists()
