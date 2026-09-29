"""Persistence is LOUD (audit 2026-09-28, d2): a discovered store that cannot hold the
records, and every refused persist write, show up on stderr AND in the root reflect's
`persistence.error` — and `persistence.provider` is reported only while writes land.
Before this, a store whose `allow` did not expose the records was still discovered,
every write was refused into the void, and the root reported it as the provider: a
silent RAM-only kernel."""

import asyncio

from kernel_state.tools import on_shutdown

from _testkit import boot_root


async def _reflect_persistence(root) -> dict:
    r = (await root.send([{"target": root.id, "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    return r["persistence"]


async def _boot(root) -> None:
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0]["result"]


async def test_a_store_that_does_not_expose_the_records_is_loud_at_boot(
    tmp_path, monkeypatch, capsys
):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["data"],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    await _boot(root)
    try:
        p = await _reflect_persistence(root)
        assert p["provider"] is None, p
        assert p["error"]["store"] == "store", p
        assert "agent.yaml" in p["error"]["path"], p
        assert "persist through 'store' failed" in capsys.readouterr().err
    finally:
        await on_shutdown(root)


async def test_a_refused_write_is_loud_and_clears_when_writes_land(
    tmp_path, monkeypatch, capsys
):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        readonly=True,
        ingress_rule="allow_all_ingress",
    )
    await _boot(root)
    try:
        assert (await _reflect_persistence(root)) == {"provider": "store"}
        root.create("file_bridge.tools", allow=["."], id="probe")
        await asyncio.sleep(0.4)  # debounce window + the refused write
        p = await _reflect_persistence(root)
        assert p["provider"] is None and p["error"]["store"] == "store", p
        assert "readonly" in str(p["error"]["error"]), p
        assert "persist through 'store' failed" in capsys.readouterr().err
        # the operator fixes the store; the next write (the store's own updated
        # record) lands and the error clears
        root.ctx.update("store", readonly=False)
        await asyncio.sleep(0.4)
        assert (await _reflect_persistence(root)) == {"provider": "store"}
        assert (tmp_path / ".fantastic" / "agents" / "store" / "agent.yaml").exists()
    finally:
        await on_shutdown(root)
