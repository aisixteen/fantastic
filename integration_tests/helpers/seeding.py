"""Workdir seeding for fantastic kernels — via one-shot CLI calls.

Both Python and Swift CLIs support `fantastic <id> <verb> [k=v ...]`
one-shot RPC against the workdir. We use that to create agents, so
the on-disk format is whatever the canonical runtime writes (nested
`agents/<id>/agents/<child>/agent.json` per Python). This avoids
hand-rolling JSON files that might diverge from the canonical shape.

The trade-off: each seed step is a subprocess invocation (~50-200 ms
overhead per call). For tests with a handful of agents that's fine.

If a test needs a hot-path bulk seed (10+ agents), bypass this and
use direct file writes — but then check the layout matches
`python/kernel/_agent.py:_load_children` exactly.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any


def seed_create(
    binary: Path,
    workdir: Path,
    *,
    handler_module: str,
    agent_id: str | None = None,
    parent_id: str = "core",
    timeout: float = 15.0,
    **meta: Any,
) -> dict[str, Any]:
    """One-shot `fantastic <parent_id> create_agent ...` against
    `workdir`. Dispatch is intentionally sent to the PARENT agent
    (not always `core`) because Python's substrate places new
    children under the calling agent's children dir, regardless of
    a `parent_id=` field in the payload. To get
    `.fantastic/agents/web/agents/web_ws/agent.json` (nested), we
    must call `fantastic web create_agent ...`.
    """
    args: list[str] = [
        parent_id, "create_agent",
        f"handler_module={handler_module}",
    ]
    if agent_id is not None:
        args.append(f"id={agent_id}")
    for k, v in meta.items():
        args.append(f"{k}={_render(v)}")

    proc = subprocess.run(
        [str(binary), *args],
        cwd=str(workdir),
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"seed_create failed (rc={proc.returncode})\n"
            f"args: {args}\nstdout: {proc.stdout}\nstderr: {proc.stderr}"
        )
    out = proc.stdout.strip()
    if not out:
        return {}
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        # Some kernels emit tracing/log lines to stdout before the
        # pretty-printed JSON reply (e.g. rust's tracing WARN on a
        # seed-time auto-boot that can't reach a not-yet-up peer).
        # The reply is the trailing JSON object — extract from the
        # last top-level `{`.
        brace = out.rfind("\n{")
        candidate = out[brace + 1 :] if brace != -1 else out[out.find("{") :]
        try:
            return json.loads(candidate)
        except (json.JSONDecodeError, ValueError) as e:
            raise RuntimeError(
                f"seed_create returned non-JSON: {out!r} ({e})"
            ) from e


def _render(v: Any) -> str:
    """Render a Python value for the CLI's `k=v` format. Bool → true/false,
    int/float → string, str → bare string. Matches Python's `_coerce`
    semantics in reverse."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    return str(v)


def seed_web(binary: Path, workdir: Path, port: int) -> None:
    """Persist a `web` agent bound to `port`. Idempotent if the
    runtime's one-shot CLI handles "agent already exists" gracefully;
    otherwise call only once per workdir.
    """
    reply = seed_create(
        binary, workdir,
        handler_module="web.tools",
        agent_id="web",
        port=port,
    )
    if "error" in reply:
        raise RuntimeError(f"web seed failed: {reply}")


def seed_web_ws(binary: Path, workdir: Path) -> None:
    """Persist a `web_ws` agent as a child of `web`. Required on BOTH
    kernels for WS — the host serves `/<id>/ws` only when a `web_ws`
    child contributes the route (WS is opt-in, parity with Python)."""
    reply = seed_create(
        binary, workdir,
        handler_module="web_ws.tools",
        agent_id="web_ws",
        parent_id="web",
    )
    if "error" in reply and "no bundle" not in str(reply.get("error", "")):
        raise RuntimeError(f"web_ws seed failed: {reply}")


def seed_web_rest(binary: Path, workdir: Path, agent_id: str = "rest") -> str:
    """Persist a `web_rest` agent as a child of `web`. Contributes
    `POST /<self>/<target>` (verb in body) + `GET /<self>/_reflect`
    routes the host mounts at boot. Returns the agent id. Works on
    both kernels (web_rest.tools is registered in each)."""
    reply = seed_create(
        binary, workdir,
        handler_module="web_rest.tools",
        agent_id=agent_id,
        parent_id="web",
    )
    if "error" in reply and "no bundle" not in str(reply.get("error", "")):
        raise RuntimeError(f"web_rest seed failed: {reply}")
    return reply.get("id", agent_id)


def seed_bridge_ws(
    binary: Path,
    workdir: Path,
    *,
    agent_id: str,
    peer_id: str,
    peer_port: int,
    host: str = "127.0.0.1",
) -> dict[str, Any]:
    """Persist a `kernel_bridge` agent for the WS transport
    (asymmetric — no peer bridge needed).

    - `agent_id`   local id for this bridge agent
    - `peer_id`    the WS path segment on the peer kernel —
                   `ws://<host>:<peer_port>/<peer_id>/ws`. Any agent
                   id that the peer's WS surface serves works
                   (typically `core`); it only selects which inbox
                   the connection auto-watches. The actual call
                   `target` travels in each forwarded frame.
    - `peer_port`  the peer kernel's web port (the bridge dials it)

    The bridge ships raw `{type:"call", target, payload}` frames; the
    peer's `web_ws` (Python) or native WS server (Swift) dispatches
    `kernel.send(target, payload)` and replies over the same socket.
    No B-side bridge agent, no read_loop on the remote.

    Note: `local_port` is the historical field name for "the port to
    dial" (shared with the ssh+ws tunnel path); here it carries
    `peer_port`.
    """
    reply = seed_create(
        binary, workdir,
        handler_module="kernel_bridge.tools",
        agent_id=agent_id,
        transport="ws",
        peer_id=peer_id,
        local_port=peer_port,
        host=host,
    )
    if "error" in reply:
        raise RuntimeError(f"ws bridge seed failed: {reply}")
    return reply
