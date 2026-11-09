"""web — one listener per web agent.

Twin of fantastic-web tests.rs::deleting_one_web_agent_frees_only_its_own_listener.
"""

from __future__ import annotations

import asyncio
import socket

from web import tools as web_tools


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


async def _connects(port: int) -> bool:
    try:
        _, w = await asyncio.open_connection("127.0.0.1", port)
    except OSError:
        return False
    w.close()
    try:
        await w.wait_closed()
    except OSError:
        pass
    return True


async def wait_accepts(port: int) -> bool:
    for _ in range(250):
        if await _connects(port):
            return True
        await asyncio.sleep(0.02)
    return False


async def _send(kernel, target: str, payload: dict) -> dict:
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def test_deleting_one_web_agent_frees_only_its_own_listener(seeded_kernel):
    k = seeded_kernel
    ids = {"web_s4a": free_port(), "web_s4b": free_port()}
    try:
        for aid, port in ids.items():
            rec = await _send(
                k,
                "kernel",
                {
                    "type": "create_agent",
                    "id": aid,
                    "handler_module": "web.tools",
                    "parent_id": k.id,
                    "port": port,
                },
            )
            assert "error" not in rec, rec
        for port in ids.values():
            assert await wait_accepts(port), "listener never came up"

        for aid in ids:
            assert (await _send(k, aid, {"type": "reflect"}))["running"] is True

        gone = await _send(k, "kernel", {"type": "delete_agent", "id": "web_s4b"})
        assert "error" not in gone, gone

        freed = False
        for _ in range(100):
            if not await _connects(ids["web_s4b"]):
                freed = True
                break
            await asyncio.sleep(0.02)
        assert freed, "the deleted agent's port still accepts"
        assert await _connects(ids["web_s4a"]), "the other listener is untouched"
        assert (await _send(k, "web_s4a", {"type": "reflect"}))["running"] is True
        assert "web_s4b" not in web_tools._servers
        assert "web_s4a" in web_tools._servers
    finally:
        try:
            await _send(k, "kernel", {"type": "delete_agent", "id": "web_s4a"})
        finally:
            for aid in ids:
                web_tools._servers.pop(aid, None)


async def _memory(k) -> None:
    k.create("ram_store.tools", id="memory")
    k.ctx.update(k.id, memory_store_id="memory")


# twin: a_recorded_web_listens_with_no_port_given (src/tui/src/kern/host.rs)
async def test_a_recorded_web_listens_with_no_port_given(seeded_kernel):
    k = seeded_kernel
    await _memory(k)
    port = free_port()
    k.create("web.tools", id="web_560", port=port)
    try:
        assert not await _connects(port)
        out = await k.ctx.start(boot_all=True)
        assert "web_560" in out["booted"], out
        assert await wait_accepts(port)
    finally:
        try:
            await _send(k, "kernel", {"type": "delete_agent", "id": "web_560"})
        finally:
            web_tools._servers.pop("web_560", None)


# twin: a_tree_without_a_web_record_boots_no_web (src/tui/src/kern/host.rs)
async def test_a_tree_without_a_web_record_boots_no_web(seeded_kernel):
    k = seeded_kernel
    await _memory(k)
    before = set(web_tools._servers)
    out = await k.ctx.start(boot_all=True)
    assert out.get("started"), out
    assert set(web_tools._servers) == before
