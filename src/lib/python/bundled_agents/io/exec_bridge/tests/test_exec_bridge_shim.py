"""exec_bridge — the WIRE is the contract; a shim is a convenience over it.

This is the difference between generic and python-shaped. `python_runtime` prepended
179 lines of Python unconditionally, which is what bound an entire executor to one
language. Here a program may ASK for a shim (`shim:'python'`), or name none and speak
the published framing itself — and adding a language is adding a blob to `SHIMS`.
"""

from __future__ import annotations

import asyncio
import os
import sys
import time

import pytest
from exec_bridge import shims


def _real_python() -> str:
    """A real interpreter for the TEST to wire onto a record's allowlist.

    `sys.executable` is the operator's choice here — which is exactly what the
    allowlist models. Skip cleanly rather than assert about a machine that has none."""
    if not (sys.executable and os.access(sys.executable, os.X_OK)):
        pytest.skip("no executable interpreter available to allowlist")
    return sys.executable


async def _make(kernel, **meta) -> str:
    rec = await kernel.send(
        kernel.id,
        {"type": "create_agent", "handler_module": "exec_bridge.tools", **meta},
    )
    assert "id" in rec, f"exec_bridge creation failed: {rec!r}"
    return rec["id"]


async def _wired(kernel, **meta) -> str:
    """An OPEN leg allowlisting THIS interpreter. `shim` stays the caller's choice."""
    return await _make(
        kernel, ingress_rule="allow_all", interpreters=[_real_python()], **meta
    )


async def _poll_done(kernel, xid: str, job_id: str, timeout: float = 10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        j = (await kernel.send(xid, {"type": "status", "job_id": job_id}))["jobs"][0]
        if j["status"] in ("done", "failed", "stopped"):
            return j
        await asyncio.sleep(0.05)
    raise AssertionError("job did not finish in time")


# A program that speaks the PUBLISHED WIRE itself — no shim, no prepended client.
# It also reports whether a `kernel` global exists, which is how a prepended shim
# would announce itself.
_RAW_WIRE = r"""
import json, os, socket, struct

_s = socket.socket(fileno=int(os.environ["FANTASTIC_CTRL_FD"]))


def _recvn(n):
    b = b""
    while len(b) < n:
        c = _s.recv(n - len(b))
        if not c:
            raise SystemExit("control fd EOF")
        b += c
    return b


def _call(obj):
    body = json.dumps(obj).encode("utf-8")
    _s.sendall(struct.pack(">I", len(body)) + body)
    (n,) = struct.unpack(">I", _recvn(4))
    return json.loads(_recvn(n).decode("utf-8"))


print("SHIMMED" if "kernel" in globals() else "NO_SHIM", flush=True)
print("SELF", os.environ["FANTASTIC_SELF_ID"], flush=True)
print("ARGS", os.environ["FANTASTIC_ARGS"], flush=True)
_r = _call({"op": "send", "rid": "7", "target": "__TARGET__",
            "payload": {"type": "reflect"}})
print("PEER", _r["data"]["id"], _r["rid"], flush=True)
"""


# ─── (a) the shim, as data ──────────────────────────────────────


def test_apply_is_a_prepend_and_nothing_else():
    """A shim must be a BLOB, not a transform: naming none returns the program byte for
    byte, and naming one leaves it intact at the tail. If `apply` could rewrite the
    program, the wire would stop being the contract."""
    prog = "print('mine')\n"
    assert shims.apply(None, prog) == prog
    assert shims.apply("", prog) == prog
    assert shims.apply("   ", prog) == prog
    assert "fantastic kernel connector" not in shims.apply(None, prog)

    shimmed = shims.apply("python", prog)
    assert shimmed.endswith(prog)
    assert "fantastic kernel connector" in shimmed
    assert shims.names() == ["python"]


def test_the_python_shim_is_imported_not_copied():
    """Two copies of a wire client is two truths for one protocol — the exact defect the
    rust/swift ports guard with a byte-equality test. While `python_runtime` still ships,
    the shim must BE its connector, not a fork of it."""
    from exec_bridge._connector import CONNECTOR_SRC

    assert shims.SHIMS["python"] is CONNECTOR_SRC


def test_an_unknown_shim_name_raises_naming_the_served_set():
    """A silent pass-through to the raw wire would fail later, elsewhere, with a
    `NameError` that names nothing useful. Refuse next to the mistake."""
    with pytest.raises(KeyError) as e:
        shims.apply("ruby", "puts 1")
    assert "unknown shim" in str(e.value) and "python" in str(e.value)


# ─── (b) the shim, end to end ───────────────────────────────────


async def test_shim_python_gives_the_program_a_working_kernel(seeded_kernel):
    """`shim:'python'` must yield a `kernel` that actually reaches the kernel — its
    whole purpose. The reply proves the full round trip child → spawner → kernel."""
    xid = await _wired(seeded_kernel, shim="python")
    peer = await _make(seeded_kernel)  # sealed: `reflect` still answers
    code = (
        "print(kernel.self_id, flush=True)\n"
        f"print(kernel.send({peer!r}, {{'type': 'reflect'}})['id'], flush=True)\n"
    )
    r = await seeded_kernel.send(xid, {"type": "start", "code": code})
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert j["status"] == "done", f"shimmed job failed: {j!r}"
    assert xid in j["stdout"], "the program must know its OWNER id, not the job id"
    assert peer in j["stdout"], "kernel.send did not reach the peer agent"


async def test_the_shim_may_be_asked_for_per_call_not_only_on_the_record(seeded_kernel):
    """`shim` on the payload is what makes one leg serve both shapes; if only the record
    could carry it, a leg would be as language-bound as the bundle it replaces."""
    xid = await _wired(seeded_kernel)  # NO shim on the record
    r = await seeded_kernel.send(
        xid,
        {"type": "start", "shim": "python", "code": "print(kernel.self_id)"},
    )
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert xid in j["stdout"], f"per-call shim not applied: {j!r}"


async def test_no_shim_leaves_the_program_unmodified(seeded_kernel):
    """The published WIRE is the contract: a program that names no shim gets the raw fd
    and the framing, with NOTHING prepended — it proves that by speaking the wire and by
    reporting that no `kernel` global was injected."""
    xid = await _wired(seeded_kernel)  # NO shim, on the record or the call
    peer = await _make(seeded_kernel)
    r = await seeded_kernel.send(
        xid,
        {
            "type": "start",
            "code": _RAW_WIRE.replace("__TARGET__", peer),
            "args": {"k": 1},
        },
    )
    j = await _poll_done(seeded_kernel, xid, r["job_id"])
    assert j["status"] == "done", f"raw-wire job failed: {j!r}"
    out = j["stdout"]
    assert "NO_SHIM" in out and "SHIMMED" not in out, "a client was prepended"
    assert f"SELF {xid}" in out, "FANTASTIC_SELF_ID must be the OWNER agent id"
    assert 'ARGS {"k": 1}' in out, "FANTASTIC_ARGS must carry this call's args"
    assert f"PEER {peer} 7" in out, "the wire's rid must come back untouched"


async def test_an_unknown_shim_is_refused_at_start_naming_the_served_set(seeded_kernel):
    """A caller asking for a language this bridge does not serve must be told which it
    does — and nothing may be spawned in the meantime."""
    xid = await _wired(seeded_kernel)
    r = await seeded_kernel.send(
        xid, {"type": "start", "shim": "ruby", "code": "puts 1"}
    )
    assert "error" in r and "unknown shim" in r["error"]
    assert "python" in r["error"], "the refusal must name the served set"
    assert "job_id" not in r, "nothing may be spawned"


async def test_an_unknown_shim_is_refused_at_the_WRITE_too(seeded_kernel):
    """`create_agent` discards the boot reply, so a record carrying an unservable shim
    would look created and fail only when someone ran it."""
    r = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "exec_bridge.tools",
            "ingress_rule": "allow_all",
            "shim": "ruby",
        },
    )
    assert "error" in r and "unknown" in r["error"]
    assert "python" in r["error"]
    assert r.get("created") is False and "id" not in r
