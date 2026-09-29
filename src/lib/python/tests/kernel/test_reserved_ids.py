"""Two ids no local agent may hold — refused at create AND at load.

`kernel` names the operator: a send to it reaches the root, and a memory store
serves the operator under it. Measured 2026-09-28: `create_agent id=kernel`
succeeded, and that agent — unreachable, since sends to `kernel` land on the root
— was the operator to every `ram_store` (its rows, and `dump`/`inject` wherever
`admins: ["kernel"]`).

`|` qualifies a FOREIGN agent (`<guid>|<agent>`). A local id never carries one,
so a gate tells local from foreign by the id alone.
"""

from __future__ import annotations

import pytest

from kernel._state import SnapshotError, validate_records


async def _create(kernel, agent_id):
    return (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "id": agent_id,
                        "handler_module": "file_bridge.tools",
                    },
                }
            ]
        )
    )[0]["result"]


@pytest.mark.parametrize("agent_id", ["kernel", "a|b", "guid|kernel"])
async def test_create_refuses_a_reserved_or_foreign_id(kernel, agent_id):
    r = await _create(kernel, agent_id)
    assert r.get("reason") == "bad_arg", r
    assert agent_id not in kernel.ctx.agents


async def test_a_plain_id_still_creates(kernel):
    assert (await _create(kernel, "kernel_notes")).get("id") == "kernel_notes"


@pytest.mark.parametrize("agent_id", ["kernel", "g|x"])
def test_a_snapshot_carrying_one_is_refused_before_the_teardown(agent_id):
    with pytest.raises(SnapshotError) as e:
        validate_records(
            [
                {"id": "root", "parent_id": None},
                {"id": agent_id, "parent_id": "root"},
            ]
        )
    assert repr(agent_id) in str(e.value)
