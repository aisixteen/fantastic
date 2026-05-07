"""kernel_bridge — cross-kernel agent comms.

Pairs of bridge agents on two kernels exchange `forward` envelopes
over a transport (memory / WS / SSH+WS). A local agent that wants
to reach a remote agent does:

    await kernel.send(local_bridge_id, {
        "type": "forward",
        "target": "<remote_agent_id>",
        "payload": {"type": "reflect"},
    })

The local bridge wraps that into a `call` frame addressed to the
peer bridge on the remote side, sends it over the transport, awaits
the matching `reply` frame (correlated by namespaced `corr_id`), and
returns the unwrapped reply.

Weak proxies: local→local agent comms remain direct
`kernel.send`. The bridge inserts itself only when the destination
is on the other side. Multi-hop loop detection is the caller's
responsibility — the bridge ships whatever it's given.

Lifecycle:
  boot      — open the transport (and SSH tunnel for ssh+ws), spawn
              the read loop, emit `bridge_up`.
  shutdown  — cancel read loop, close transport, kill tunnel pid,
              reject pending Futures. Called automatically by
              core.delete_agent's universal `shutdown` hook.
  reconnect — explicit shutdown + boot. No auto-reconnect (keeps
              real failures visible in the substrate).

Reply correlation: `corr_id = f"{bridge_id}:{counter}"` — the
namespace eliminates collisions across bridges. The frame `id` is
set to the same value so logs across hops correlate trivially.
"""

from __future__ import annotations

import asyncio
import os
import signal as signal_mod
import socket
import subprocess
from dataclasses import dataclass, field
from typing import Any

from kernel_bridge._transport import (
    ConnectionClosed,
    WSTransport,
    _BaseTransport,
)

DEFAULT_FORWARD_TIMEOUT = 30.0
TUNNEL_READY_TIMEOUT = 5.0


@dataclass
class _BridgeState:
    transport: _BaseTransport | None = None
    # Empty until `boot` succeeds — reflect falls through to the
    # record's `transport` field for the advertised config.
    transport_kind: str = ""  # 'memory' | 'ws' | 'ssh+ws' once booted
    read_task: asyncio.Task | None = None
    tunnel_proc: subprocess.Popen | None = None
    tunnel_pid: int | None = None
    pending: dict[str, asyncio.Future] = field(default_factory=dict)
    corr_counter: int = 0


# Per-agent bridge state — process-memory only (mirrors
# terminal_backend._procs). Survives across calls within one kernel
# process; lost on restart, which is correct (a fresh kernel re-boots
# bridges via their `boot` verb).
_bridges: dict[str, _BridgeState] = {}

# Test seam: MemoryTransport tests inject a pre-built transport into
# the state before booting, since memory pairs can't be discovered
# from agent.json fields. Real ws / ssh+ws bridges build their
# transport in `_boot` from record fields.
_test_transport_inject: dict[str, _BaseTransport] = {}


# ─── helpers ────────────────────────────────────────────────────


def _state(id: str) -> _BridgeState:
    s = _bridges.get(id)
    if s is None:
        s = _BridgeState()
        _bridges[id] = s
    return s


def _next_corr(id: str, st: _BridgeState) -> str:
    st.corr_counter += 1
    return f"{id}:{st.corr_counter}"


async def _open_tunnel(
    host: str, local_port: int, remote_port: int
) -> subprocess.Popen:
    """Spawn `ssh -L local:localhost:remote -N -f-equivalent <host>`.

    No `-f` (fork) — we keep the Popen handle so we can SIGTERM it
    on shutdown. ServerAliveInterval keeps the tunnel from silently
    dying behind a stateful firewall; ExitOnForwardFailure makes ssh
    exit immediately if the local port is already bound (so we
    notice a port conflict instead of a half-open tunnel).
    """
    cmd = [
        "ssh",
        "-N",
        "-L",
        f"{local_port}:localhost:{remote_port}",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        host,
    ]
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    # Wait for the local port to actually accept connections — this
    # is the real readiness signal (ssh exits non-zero on auth/route
    # failures, which we surface as a tunnel-not-ready error).
    deadline = asyncio.get_event_loop().time() + TUNNEL_READY_TIMEOUT
    while asyncio.get_event_loop().time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"ssh tunnel exited early (code {proc.returncode})")
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.2)
                s.connect(("127.0.0.1", local_port))
                return proc
        except OSError:
            await asyncio.sleep(0.1)
    proc.terminate()
    raise TimeoutError(
        f"ssh tunnel to {host}:{remote_port} not ready in {TUNNEL_READY_TIMEOUT}s"
    )


def _kill_tunnel(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal_mod.SIGTERM)
    except OSError:
        try:
            proc.terminate()
        except OSError:
            return
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal_mod.SIGKILL)
        except OSError:
            try:
                proc.kill()
            except OSError:
                pass


async def _read_loop(id: str, kernel: Any) -> None:
    """Long-lived consumer of the transport. Two frame shapes:

      - inbound `call` whose payload is a `forward` envelope
        addressed at this kernel: unwrap, kernel.send, send `reply`
        back wrapping the result;
      - inbound `reply` with `id` matching one of our pending
        outbound corr_ids: resolve the Future.

    On ConnectionClosed (peer dropped, ssh tunnel collapsed): emit
    `{type:'bridge_down'}` on the bridge agent's own inbox so
    canvas/telemetry subscribers see it, then fail every pending
    Future with ConnectionError. Caller invokes `reconnect` to
    re-establish.
    """
    st = _state(id)
    transport = st.transport
    assert transport is not None
    try:
        while True:
            try:
                frame = await transport.recv()
            except ConnectionClosed:
                break
            ftype = frame.get("type")
            if ftype == "call":
                # Inbound forward envelope from the peer bridge.
                payload = frame.get("payload") or {}
                if payload.get("type") != "forward":
                    # Unknown call shape — not addressed at our
                    # forward dispatcher. Reply with an error so the
                    # peer's pending Future resolves cleanly.
                    await transport.send(
                        {
                            "type": "reply",
                            "id": frame.get("id"),
                            "data": {
                                "error": f"kernel_bridge: unknown call payload type {payload.get('type')!r}"
                            },
                        }
                    )
                    continue
                target = payload.get("target")
                inner = payload.get("payload") or {}
                try:
                    reply = await kernel.send(target, inner)
                except Exception as e:
                    reply = {"error": f"kernel_bridge: kernel.send raised: {e}"}
                await transport.send(
                    {
                        "type": "reply",
                        "id": frame.get("id"),
                        "data": reply,
                    }
                )
            elif ftype == "reply":
                fut = st.pending.pop(frame.get("id"), None)
                if fut is not None and not fut.done():
                    fut.set_result(frame.get("data"))
            # Other frame types (event etc.) are not used by the
            # bridge protocol; ignore so the loop stays robust.
    finally:
        # Surface drop, fail pending, leave state in a re-bootable
        # shape (transport=None) so reconnect works.
        try:
            await kernel.emit(id, {"type": "bridge_down"})
        except Exception:
            pass
        for cid, fut in list(st.pending.items()):
            if not fut.done():
                fut.set_exception(ConnectionError("bridge transport closed"))
        st.pending.clear()


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + transport + connectivity. No args."""
    rec = kernel.get(id) or {}
    st = _state(id)
    return {
        "id": id,
        "sentence": "Cross-kernel comms bridge — pairs over WS/memory; weak proxy.",
        "transport": st.transport_kind or rec.get("transport") or "ws",
        "connected": st.transport is not None and not st.transport.closed,
        "host": rec.get("host"),
        "peer_id": rec.get("peer_id"),
        "local_port": rec.get("local_port"),
        "remote_port": rec.get("remote_port"),
        "tunnel_pid": st.tunnel_pid,
        "pending_count": len(st.pending),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "bridge_up": "{type:'bridge_up'} — emitted on this agent's inbox after a successful boot",
            "bridge_down": "{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)",
        },
    }


async def _boot(id, payload, kernel):
    """No args. Reads `transport` (memory|ws|ssh+ws), `peer_id`, plus transport-specific fields off the agent record. Builds the transport, spawns the read loop, emits `bridge_up`. Idempotent: re-booting a connected bridge is a no-op."""
    rec = kernel.get(id) or {}
    st = _state(id)
    if st.transport is not None and not st.transport.closed:
        return {"already": True, "transport": st.transport_kind}

    kind = rec.get("transport") or "ws"
    peer_id = rec.get("peer_id")
    if not peer_id and kind != "memory":
        return {"error": "kernel_bridge: peer_id required for ws/ssh+ws transports"}

    transport: _BaseTransport
    if kind == "memory":
        # Tests inject the transport via _test_transport_inject before
        # boot (no agent.json field can carry a Queue).
        if id not in _test_transport_inject:
            return {"error": "kernel_bridge: memory transport requires test injection"}
        transport = _test_transport_inject.pop(id)
    elif kind == "ws":
        port_val = rec.get("local_port") or rec.get("remote_port")
        if not port_val:
            return {
                "error": "kernel_bridge: ws transport requires local_port or remote_port"
            }
        local_port = int(port_val)
        host = rec.get("host") or "localhost"
        url = f"ws://{host}:{local_port}/{peer_id}/ws"
        try:
            transport = await WSTransport.connect(url)
        except Exception as e:
            return {"error": f"kernel_bridge: ws connect failed: {e}"}
    elif kind == "ssh+ws":
        host = rec.get("host")
        local_port = int(rec.get("local_port") or 0)
        remote_port_val = rec.get("remote_port")
        if not host or not local_port or not remote_port_val:
            return {
                "error": "kernel_bridge: ssh+ws requires host, local_port, remote_port"
            }
        remote_port = int(remote_port_val)
        try:
            tunnel = await _open_tunnel(host, local_port, remote_port)
        except Exception as e:
            return {"error": f"kernel_bridge: tunnel failed: {e}"}
        st.tunnel_proc = tunnel
        st.tunnel_pid = tunnel.pid
        try:
            transport = await WSTransport.connect(
                f"ws://localhost:{local_port}/{peer_id}/ws"
            )
        except Exception as e:
            _kill_tunnel(tunnel)
            st.tunnel_proc = None
            st.tunnel_pid = None
            return {"error": f"kernel_bridge: ws over tunnel failed: {e}"}
    else:
        return {"error": f"kernel_bridge: unknown transport {kind!r}"}

    st.transport = transport
    st.transport_kind = kind
    st.read_task = asyncio.create_task(_read_loop(id, kernel))
    await kernel.emit(id, {"type": "bridge_up"})
    return {"booted": True, "transport": kind, "tunnel_pid": st.tunnel_pid}


async def _shutdown(id, payload, kernel):
    """No args. Cancels the read loop, closes the transport, kills the SSH tunnel (if any), rejects pending Futures. Called automatically by core.delete_agent's universal lifecycle hook."""
    st = _bridges.get(id)
    if st is None:
        return {"shutdown": True, "noop": True}
    if st.read_task is not None and not st.read_task.done():
        st.read_task.cancel()
        try:
            await st.read_task
        except (asyncio.CancelledError, Exception):
            pass
    if st.transport is not None:
        try:
            await st.transport.close()
        except Exception:
            pass
    _kill_tunnel(st.tunnel_proc)
    for fut in list(st.pending.values()):
        if not fut.done():
            fut.set_exception(ConnectionError("bridge shut down"))
    st.pending.clear()
    st.transport = None
    st.read_task = None
    st.tunnel_proc = None
    st.tunnel_pid = None
    return {"shutdown": True}


async def _reconnect(id, payload, kernel):
    """No args. Equivalent to shutdown + boot — explicit because we don't auto-reconnect on transport failure (keeps real network problems visible to operators / telemetry)."""
    await _shutdown(id, payload, kernel)
    return await _boot(id, payload, kernel)


async def _forward(id, payload, kernel):
    """args: target:str (req — id on the REMOTE kernel), payload:dict (req), timeout:float? (default 30s). Wraps + ships the inner payload to the peer bridge, awaits the matching reply, returns the unwrapped result. Local→local stays direct kernel.send (this verb is only for cross-kernel). Multi-hop loop detection is the caller's responsibility — the bridge ships whatever it's given."""
    target = payload.get("target")
    inner = payload.get("payload")
    if not target or not isinstance(inner, dict):
        return {
            "error": "kernel_bridge.forward: target (str) + payload (dict) required"
        }
    timeout = float(payload.get("timeout", DEFAULT_FORWARD_TIMEOUT))

    st = _state(id)
    if st.transport is None or st.transport.closed:
        return {"error": "kernel_bridge.forward: not connected (call boot first)"}

    rec = kernel.get(id) or {}
    peer_id = rec.get("peer_id")

    corr = _next_corr(id, st)
    fut: asyncio.Future = asyncio.get_event_loop().create_future()
    st.pending[corr] = fut

    frame = {
        "type": "call",
        "target": peer_id,
        "payload": {
            "type": "forward",
            "target": target,
            "payload": inner,
            "corr_id": corr,
        },
        "id": corr,  # frame.id == corr_id — single layer of correlation
    }
    try:
        await st.transport.send(frame)
    except ConnectionClosed as e:
        st.pending.pop(corr, None)
        return {"error": f"kernel_bridge.forward: send failed: {e}"}

    try:
        return await asyncio.wait_for(fut, timeout=timeout)
    except asyncio.TimeoutError:
        st.pending.pop(corr, None)
        return {"error": f"kernel_bridge.forward: timeout after {timeout}s"}
    except ConnectionError as e:
        return {"error": f"kernel_bridge.forward: {e}"}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "shutdown": _shutdown,
    "reconnect": _reconnect,
    "forward": _forward,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"kernel_bridge: unknown type {t!r}"}
    return await fn(id, payload, kernel)
