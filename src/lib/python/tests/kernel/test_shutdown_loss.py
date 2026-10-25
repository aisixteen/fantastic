"""A record updated just before shutdown always lands.

Rust twin: fantastic-kernel tests/shutdown_loss.rs::a_record_updated_just_before_shutdown_always_lands.
The record writer's final flush runs in the root bundle's `flush_pending` hook
- before the teardown walk, while the store is alive - so a create+update
immediately followed by a shutdown is on disk, not lost to the debounce.
"""

from __future__ import annotations

import yaml

from kernel_state.tools import on_shutdown

from _testkit import boot_root


# Rust twin: tests/shutdown_loss.rs::a_record_updated_just_before_shutdown_always_lands
async def test_a_record_updated_just_before_shutdown_always_lands(
    tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])
    try:
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "sp_late",
                    },
                }
            ]
        )
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "sp_late",
                        "can_call": {"store": "*"},
                    },
                }
            ]
        )
        await root.ctx.shutdown()  # no sleep: the debounce has not fired
    finally:
        await on_shutdown(root)
    f = tmp_path / ".fantastic" / "agents" / "sp_late" / "agent.yaml"
    assert f.exists(), "the record never reached disk"
    rec = yaml.safe_load(f.read_text(encoding="utf-8"))
    assert rec["can_call"]["store"] == "*"
