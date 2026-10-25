"""web host — twins of the Rust `fantastic-web` tests (#170).

Rust is the spec; each test is named `test_<rust_test>`. A booted host listens on
a real loopback port (`create_agent` boots it), and every test deletes it.
"""

from __future__ import annotations

import asyncio
import socket
import types

import httpx
import pytest
import websockets
from fastapi.testclient import TestClient

from kernel import register_unlisted
from web import tools as web_tools
from web.tools import VERBS, _reflect
from _testkit import wire


def _free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


async def _wait_accepts(port: int) -> None:
    for _ in range(250):
        try:
            _, w = await asyncio.open_connection("127.0.0.1", port)
        except OSError:
            await asyncio.sleep(0.02)
            continue
        w.close()
        return
    raise AssertionError(f"port {port} never accepted")


async def _send(kernel, target: str, payload: dict) -> dict:
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _create(kernel, **meta) -> dict:
    rec = await _send(
        kernel, kernel.id, {"type": "create_agent", "parent_id": kernel.id, **meta}
    )
    assert rec.get("id") == meta["id"], rec
    return rec


async def _child(kernel, web_id: str, module: str, cid: str, **meta) -> None:
    """A leg under the host, whose `get_routes` the host may pull."""
    await _create(
        kernel,
        id=cid,
        handler_module=module,
        parent_id=web_id,
        ingress_rule="allow_all_ingress",
        **meta,
    )
    wire(kernel, web_id, cid, ["get_routes"])


async def _drop(kernel, web_id: str) -> None:
    await _send(kernel, kernel.id, {"type": "delete_agent", "id": web_id})
    web_tools._servers.pop(web_id, None)


@pytest.fixture
async def staged(seeded_kernel):
    """A booted host `web_r` on a free port with a mounted `rest` leg and one
    file_bridge `fb_r` the browser may fetch."""
    k = seeded_kernel
    port = _free_port()
    await _create(k, id="web_r", handler_module="web.tools", port=port)
    await _child(k, "web_r", "web_rest.tools", "rest")
    await _create(
        k,
        id="fb_r",
        handler_module="file_bridge.tools",
        allow=["."],
        root=".",
        ingress_rule="allow_all_ingress",
        callable_by={"web_r": ["read_stream"]},
    )
    wire(k, "web_r", "fb_r", ["read_stream"])
    yield k, port
    await _drop(k, "web_r")


class _Kernel:
    """Just enough kernel for `_reflect` on a record nothing serves."""

    def __init__(self, rec):
        self._rec = rec
        self.ctx = types.SimpleNamespace(agents={})

    def get(self, _id):
        return self._rec

    def list(self):
        return []


async def test_reflect_lists_own_sorted_then_extra():
    rec = {
        "port": 1,
        "allowed_origins": ["https://ui.example", "http://localhost:1"],
    }
    out = await _reflect("w", {}, _Kernel(rec))
    assert out["allowed_origins"] == [
        "http://127.0.0.1:1",
        "http://[::1]:1",
        "http://localhost:1",
        "https://ui.example",
    ]


async def test_web_alone_has_no_ws_endpoint(seeded_kernel):
    k = seeded_kernel
    port = _free_port()
    await _create(k, id="web_alone", handler_module="web.tools", port=port)
    try:
        await _wait_accepts(port)
        # `web` alone (no web_ws child) exposes no `/<id>/ws`: the surface-less
        # router answers 404
        async with httpx.AsyncClient() as c:
            r = await c.get(f"http://127.0.0.1:{port}/web_alone/ws")
        assert r.status_code == 404, r.text
    finally:
        await _drop(k, "web_alone")


async def test_reflect_sentence_and_verbs_match_python(staged):
    k, port = staged
    await _send(k, "web_r", {"type": "boot"})
    obj = await _send(k, "web_r", {"type": "reflect"})
    assert list(obj)[:10] == [
        "id",
        "sentence",
        "port",
        "host",
        "allowed_origins",
        "running",
        "served_agent_count",
        "surfaces",
        "routes",
        "browser_reachable",
    ], list(obj)
    assert (
        obj["sentence"]
        == "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents."
    )
    assert obj["port"] == port
    assert obj["running"] is True
    assert obj["served_agent_count"] == len(k.list())
    assert obj["surfaces"] == {
        "rest": ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"]
    }
    assert obj["browser_reachable"] == ["fb_r"]
    assert obj["routes"]["GET /<agent_id>/file/<path>"].startswith(
        "Streams a file by read_stream-ing through the named agent."
    )
    assert list(VERBS) == ["reflect", "boot", "stop", "mount", "unmount", "fetch_file"]
    assert VERBS["reflect"].__doc__ == (
        "Identity + uvicorn port + running flag + currently mounted surfaces "
        "(process-local; reflect the live serve for truth)."
    )


async def test_boot_and_stop_replies_match_python(staged):
    k, _ = staged
    assert await _send(k, "web_r", {"type": "boot"}) == {
        "running": True,
        "surfaces": ["rest"],
    }
    assert await _send(k, "web_r", {"type": "stop"}) == {"stopped": True}
    assert await _send(k, "web_r", {"type": "stop"}) == {"stopped": False}

    # A record with no `port` RAISES — no default, no fallback.
    await _create(k, id="web_noport", handler_module="web.tools")
    try:
        boot = await _send(k, "web_noport", {"type": "boot"})
        assert boot["raised"] is True, boot
        assert boot["error"] == (
            "RuntimeError: webapp web_noport: rec.port is required (no default). "
            "Set port on create_agent or via update_agent."
        )
    finally:
        await _drop(k, "web_noport")


async def test_mount_unmount_and_unknown_verb_match_python(staged):
    k, _ = staged

    async def call(payload):
        return await _send(k, "web_r", payload)

    assert await call({"type": "mount"}) == {"error": "web.mount: child_id required"}
    assert await call({"type": "unmount"}) == {
        "error": "web.unmount: child_id required"
    }
    assert await call({"type": "mount", "child_id": "rest"}) == {
        "mounted": ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"]
    }
    assert await call({"type": "unmount", "child_id": "rest"}) == {"unmounted": "rest"}
    for verb in ("shutdown", "render_html"):
        assert await call({"type": verb}) == {
            "error": f"web: unknown type {verb!r}",
            "reason": "unhandled",
            "verb": verb,
        }
    assert await call({"type": "stop"}) == {"stopped": True}
    assert await call({"type": "mount", "child_id": "rest"}) == {
        "error": "web.mount: not running"
    }
    assert await call({"type": "unmount", "child_id": "rest"}) == {
        "error": "web.unmount: not running"
    }


async def test_a_child_created_after_boot_mounts_only_through_mount(seeded_kernel):
    k = seeded_kernel
    port = _free_port()
    web_id = "web_hot"
    await _create(k, id=web_id, handler_module="web.tools", port=port)
    try:
        # Unmount before boot is refused, not silently ignored.
        assert await _send(k, web_id, {"type": "stop"}) == {"stopped": True}
        assert await _send(k, web_id, {"type": "unmount", "child_id": "ws_hot"}) == {
            "error": "web.unmount: not running"
        }
        assert (await _send(k, web_id, {"type": "boot"}))["running"] is True
        await _wait_accepts(port)

        # A child created AFTER boot is not routed until it is mounted.
        await _child(k, web_id, "web_ws.tools", "ws_hot")
        await asyncio.sleep(0.1)
        ws_url = f"http://127.0.0.1:{port}/{web_id}/ws"
        async with httpx.AsyncClient() as c:
            assert (await c.get(ws_url)).status_code == 404, "no auto-remount"

            assert await _send(k, web_id, {"type": "mount", "child_id": "ws_hot"}) == {
                "mounted": ["/{host_id}/ws"]
            }
            async with websockets.connect(f"ws://127.0.0.1:{port}/{web_id}/ws"):
                pass  # the upgrade is served after the mount

            assert await _send(
                k, web_id, {"type": "unmount", "child_id": "ws_hot"}
            ) == {"unmounted": "ws_hot"}
            assert (await c.get(ws_url)).status_code == 404, "unmount is immediate"
    finally:
        await _drop(k, web_id)


# ─── the file proxy reads on until a TRUTHY eof ─────────────────────────────

_odd = types.ModuleType("odd_source.tools")


async def _odd_handler(id, payload, kernel):
    if payload.get("type") != "read_stream":
        return None
    offset = payload.get("offset") or 0
    if id == "chunked":
        if offset == 0:  # `eof: 0` is FALSY: read on at next_offset
            return {"bytes": b"abc", "eof": 0, "next_offset": 3, "size": 9}
        if offset == 3:  # no `eof` key at all: read on
            return {"bytes": b"def", "next_offset": 6, "size": 9}
        return {"bytes": b"ghi", "eof": 1, "size": 9}  # `1` is truthy: stop
    # "broken": no `eof`, no `next_offset` — the stream cannot go on
    return {"bytes": b"abc", "size": 6}


_odd.handler = _odd_handler
_odd.VERBS = {}
register_unlisted("odd_source.tools", _odd)


async def test_the_file_proxy_reads_on_until_a_truthy_eof(seeded_kernel):
    from web.app import make_app

    k = seeded_kernel
    await _create(k, id="web_odd", handler_module="web.tools", port=8099)
    web_tools._servers.pop("web_odd", None)
    for name in ("chunked", "broken"):
        await _create(k, id=name, handler_module="odd_source.tools")
        wire(k, "web_odd", name)
    app = make_app("web_odd", k)
    with TestClient(app, base_url="http://127.0.0.1:8099") as c:
        ok = c.get("/chunked/file/x.bin")
        assert ok.status_code == 200
        assert ok.content == b"abcdefghi", "a chunk was never read"

        # A chunk with no `next_offset` aborts the body mid-stream; it is never
        # served as if whole.
        with pytest.raises(KeyError):
            c.get("/broken/file/x.bin")
