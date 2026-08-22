"""Two-tree integration — host kernel ⇄ frontend kernel, full round-trip.

Wires a HOST kernel (root `kernel_state`) + a real `web_loader` (an `kernel_state`
child with `root=.fantastic/web watch=false alias=web_loader`) + a FRONTEND
stand-in `Kernel`. Faithful because the JS kernel is the SAME kernel — a second
`Kernel` is a true stand-in for the browser. The frontend persists its tree
through `web_loader` (the real verb contract), re-rooting exactly like the JS
`ProxyLoader`. Proves: the frontend tree lands on host disk under
`.fantastic/web/`, rehydrates into a fresh frontend kernel identical (incl. a
`*.ts` record round-tripping intact), and the host MAIN kernel never sees the
frontend records — two namespaces, one link.
"""

from __future__ import annotations

from pathlib import Path

from _testkit import boot_root
from kernel_state.tools import read_tree
from kernel import Kernel


def _persist_reroot(record: dict, loader_id: str) -> dict:
    """Mirror ProxyLoader.flush: the JS root (parent_id None) lands as a direct
    child of the loader's namespace."""
    r = dict(record)
    r["parent_id"] = r.get("parent_id") or loader_id
    return r


def _load_reroot(records: list[dict], loader_id: str) -> list[dict]:
    """Mirror ProxyLoader.loadTree: drop the namespace anchor; re-root the JS
    root (parent_id == loader_id → None)."""
    out: list[dict] = []
    for rec in records:
        if rec["id"] == loader_id:
            continue
        r = dict(rec)
        if r.get("parent_id") == loader_id:
            r["parent_id"] = None
        out.append(r)
    return out


async def _host_with_web_loader():
    """Host root + a real `web_loader` (created → booted → anchor seeded +
    alias registered). Returns (host_root_agent, web_loader_id)."""
    host = boot_root()
    wl = await host.send(
        host.id,
        {
            "type": "create_agent",
            "handler_module": "kernel_state.tools",
            "root": ".fantastic/web",
            "watch": False,
            "alias": "web_loader",
        },
    )
    return host, wl["id"]


async def test_two_tree_roundtrip(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    host, wl_id = await _host_with_web_loader()

    # ── FRONTEND stand-in: build a small tree (bare view stubs + meta) ──
    fe = Kernel()
    fe.load(
        [
            {"id": "canvas"},
            {"id": "panel", "parent_id": "canvas", "html": "<p>x</p>"},
            {"id": "term", "parent_id": "canvas", "backend_id": "tb_1"},
        ]
    )

    # ── PERSIST each record through web_loader via the ALIAS (re-rooted) ──
    for rec in fe.save()["records"]:
        reply = await host.send(
            "web_loader",
            {"type": "persist_record", "record": _persist_reroot(rec, wl_id)},
        )
        assert reply.get("ok") is True
    # a `*.ts` record persists too — the loader is a dumb store; the host never
    # runs it (proves frontend bundle records round-trip intact).
    await host.send(
        "web_loader",
        {
            "type": "persist_record",
            "record": {
                "id": "vfx",
                "handler_module": "gl_agent.ts",
                "parent_id": "canvas",
            },
        },
    )

    # ── on host disk, under the web/ namespace ──
    web = tmp_path / ".fantastic" / "web" / "agents"
    assert (web / "canvas" / "agent.json").exists()
    assert (web / "canvas" / "agents" / "panel" / "agent.json").exists()
    assert (web / "canvas" / "agents" / "vfx" / "agent.json").exists()

    # ── REHYDRATE: a fresh frontend kernel loads via web_loader.load_tree ──
    reply = await host.send("web_loader", {"type": "load_tree"})
    records = _load_reroot(reply["records"], wl_id)
    by_id = {r["id"]: r for r in records}
    assert by_id["vfx"]["handler_module"] == "gl_agent.ts"  # *.ts round-tripped intact
    assert by_id["canvas"].get("parent_id") is None  # re-rooted

    fe2 = Kernel()
    fe2.load(
        records
    )  # vfx (gl_agent.ts) weak-loads in PYTHON — a JS kernel would run it
    assert fe2.root.id == "canvas"
    assert set(fe2.agents) == {"canvas", "panel", "term"}  # vfx skipped (weak-load)
    assert fe2.get("panel")["html"] == "<p>x</p>"
    assert fe2.get("term")["backend_id"] == "tb_1"
    assert "canvas" in fe2.get("panel")["parent_id"]

    # ── NAMESPACE SEPARATION: the host MAIN kernel never holds these records ──
    listed = await host.send(host.id, {"type": "list_agents"})
    host_ids = {a["id"] for a in listed["agents"]}
    assert wl_id in host_ids  # the loader itself IS a host agent
    assert not ({"canvas", "panel", "term", "vfx"} & host_ids)
    # host MAIN read_tree walks agents/, never the web/ namespace
    main_ids = {r["id"] for r in read_tree(Path(".fantastic"))}
    assert not ({"canvas", "panel", "term", "vfx"} & main_ids)


async def test_two_tree_forget(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    host, wl_id = await _host_with_web_loader()
    await host.send(
        "web_loader",
        {"type": "persist_record", "record": {"id": "canvas", "parent_id": wl_id}},
    )
    await host.send(
        "web_loader",
        {"type": "persist_record", "record": {"id": "child", "parent_id": "canvas"}},
    )
    child_dir = (
        tmp_path / ".fantastic" / "web" / "agents" / "canvas" / "agents" / "child"
    )
    assert child_dir.exists()
    # forget the nested child (parent_id resolves the path — it's no longer live)
    await host.send(
        "web_loader", {"type": "forget_record", "id": "child", "parent_id": "canvas"}
    )
    assert not child_dir.exists()
    # the parent survives + still loads
    reply = await host.send("web_loader", {"type": "load_tree"})
    assert "canvas" in {r["id"] for r in reply["records"]}
    assert "child" not in {r["id"] for r in reply["records"]}
