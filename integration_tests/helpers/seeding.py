"""Workdir seeding for fantastic kernels — via one-shot CLI calls.

Every host runtime's CLI runs `fantastic_kernel <id> <verb> [k=v ...]` as a
one-shot OPERATOR call against the workdir (no daemon up; the operator is
ungated). We use that to create agents and write their grants, so the on-disk
format is whatever the runtime itself writes (`.fantastic/agent.yaml` for the
root, nested `agents/<id>/agents/<child>/agent.yaml` below it). This avoids
hand-rolling records that might diverge from the canonical shape, and it runs
inside the container under `FANTASTIC_TARGET=container` (records owned by the
container's uid).

The kernel is DENY-ALL: every agent is stamped `{$self: "*"}` on both
`can_call` and `callable_by`, and every other edge is an explicit grant on both
ends. The seeds below grant exactly the edges the harness drives.

The trade-off: each seed step is a subprocess invocation (~50-200 ms
overhead per call). For tests with a handful of agents that's fine.
"""

from __future__ import annotations

import json
import re
import zipfile
from pathlib import Path
from typing import Any

from .launcher import Launcher, as_launcher

_ROOT_ID_CACHE: dict[str, str] = {}
_RUNTIME_CACHE: dict[str, str | None] = {}

# Repo root: integration_tests/helpers -> integration_tests -> repo.
_REPO_ROOT = Path(__file__).resolve().parents[2]

# Sovereign frontend artifact built by `cd ts && sh scripts/pack.sh`. Holds
# exactly three entries: readme.md, bundle.min.js, bundle.min.js.map.
FRONTEND_ZIP = _REPO_ROOT / "src" / "lib" / "ts" / "dist" / "js_kernel.zip"
# Pinned line in the zip's readme.md carrying the bundle's integrity digest,
# e.g. `# expected: 3881c729...`. pack.sh substitutes the real sha at build.
_SHA_LINE = re.compile(r"^#\s*expected:\s*([0-9a-f]{64})\s*$", re.MULTILINE)


def frontend_zip() -> Path:
    """Path to the sovereign frontend artifact `ts/dist/js_kernel.zip`.

    Build artifact (like the kernel binaries) — callers should
    `pytest.skip` cleanly when it's absent; use `frontend_zip().exists()`
    or the convenience message below.
    """
    return FRONTEND_ZIP


def pull_member_from_zip(zip_path: Path, member: str, dest: Path) -> bytes:
    """Stream ONE member out of `zip_path` to `dest` WITHOUT a full unzip.

    Direct-pull model: open the central directory, read just `member`,
    write its raw bytes to `dest` (parents created). Never extracts the
    whole archive, never builds an unpacked tree. Returns the member's
    bytes so the caller can hash/inspect them without re-reading disk.
    """
    with zipfile.ZipFile(zip_path) as zf:
        data = zf.read(member)
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(data)
    return data


def read_member_text(zip_path: Path, member: str) -> str:
    """Read one text member out of `zip_path` (no extraction). Used to
    pull `readme.md` and scrape its integrity line."""
    with zipfile.ZipFile(zip_path) as zf:
        return zf.read(member).decode("utf-8", errors="replace")


def expected_bundle_sha(zip_path: Path) -> str:
    """Scrape the `# expected: <sha256>` integrity line out of the zip's
    own readme.md (pulled, not extracted). Raises if the line is absent —
    a malformed/unpacked-by-hand artifact should fail loudly, not skip."""
    text = read_member_text(zip_path, "readme.md")
    m = _SHA_LINE.search(text)
    if not m:
        raise RuntimeError(
            f"no `# expected: <sha256>` integrity line in {zip_path}!readme.md "
            f"(rebuild: cd ts && sh scripts/pack.sh)"
        )
    return m.group(1)


def root_id(binary: Launcher | Path, workdir: Path, *, timeout: float = 15.0) -> str:
    """The runtime's ROOT agent id, read off a one-shot `reflect` (the root is
    the persistence loader, `kernel_state`, on every runtime). Seeds that attach
    to the root — and the grants that name it — use this. Cached per
    (binary, workdir).

    `binary` may be a launcher (local or container) or a raw binary path —
    `as_launcher` coerces a path.
    """
    key = f"{binary}|{workdir}"
    if key not in _ROOT_ID_CACHE:
        proc = as_launcher(binary).cli(workdir, ["reflect"], timeout=timeout)
        out = proc.stdout
        try:
            obj = json.loads(out[out.index("{") :])
            rid = obj["id"]
        except (ValueError, KeyError) as e:
            raise RuntimeError(
                f"root reflect gave no id (rc={proc.returncode})\n"
                f"stdout: {out}\nstderr: {proc.stderr}"
            ) from e
        _ROOT_ID_CACHE[key] = rid
        _RUNTIME_CACHE[key] = obj.get("runtime")
    return _ROOT_ID_CACHE[key]


def runtime(binary: Path, workdir: Path, *, timeout: float = 15.0) -> str | None:
    """Discover the runtime enum (`python`/`rust`/`swift`/`ts`) from the root
    reflect. Cached alongside `root_id` (one reflect feeds both)."""
    key = f"{binary}|{workdir}"
    if key not in _RUNTIME_CACHE:
        root_id(binary, workdir, timeout=timeout)
    return _RUNTIME_CACHE.get(key)


def seed_create(
    binary: Path,
    workdir: Path,
    *,
    handler_module: str,
    agent_id: str | None = None,
    parent_id: str | None = None,
    timeout: float = 15.0,
    **meta: Any,
) -> dict[str, Any]:
    """One-shot `fantastic kernel create_agent parent_id=<parent> ...` against
    `workdir`. `create_agent` is a kernel verb — addressed to the root
    (`kernel`) — and `parent_id` (required) names the owner: the root by
    default, or e.g. `web` to nest `web_ws` under it.
    """
    if parent_id is None:
        parent_id = root_id(binary, workdir, timeout=timeout)
    args: list[str] = [
        "kernel",
        "create_agent",
        f"parent_id={parent_id}",
        f"handler_module={handler_module}",
    ]
    if agent_id is not None:
        args.append(f"id={agent_id}")
    for k, v in meta.items():
        args.append(f"{k}={_render(v)}")
    return _oneshot(binary, workdir, args, timeout)


def _render(v: Any) -> str:
    """Render a Python value for the CLI's `k=v` format. Bool → true/false,
    int/float → string, str → bare string. Matches Python's `_coerce`
    semantics in reverse; a list/dict renders as JSON."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, (list, dict)):
        return json.dumps(v)
    return str(v)


#: The baseline every agent is stamped with, on BOTH `can_call` (egress) and
#: `callable_by` (ingress): itself, nothing else. Every other edge is an explicit
#: grant, written on both ends (the sender's `can_call` AND the target's
#: `callable_by`) — the deny-all call gate.
BASE: dict[str, Any] = {"$self": "*"}

#: The WS leg every `KernelProc.call` rides (`seed_web_ws`). One leg is one
#: principal: every frame the harness sends is performed AS this agent, so it is
#: the id each target's `callable_by` must name.
DRIVER = "web_ws"

#: The kernel's memory store (`seed_memory`), named by the root's `memory_store_id`.
MEMORY = "memory"


def _oneshot(binary: Launcher | Path, workdir: Path, args: list[str], timeout: float) -> Any:
    """Run one OPERATOR one-shot (`fantastic <args>`, no daemon up) and parse its
    JSON reply. Raise on a non-zero exit."""
    proc = as_launcher(binary).cli(workdir, args, timeout=timeout)
    if proc.returncode != 0:
        raise RuntimeError(
            f"one-shot failed (rc={proc.returncode})\n"
            f"args: {args}\nstdout: {proc.stdout}\nstderr: {proc.stderr}"
        )
    out = proc.stdout.strip()
    if not out:
        return {}
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        # Some kernels emit tracing/log lines to stdout before the pretty-printed
        # JSON reply (e.g. rust's tracing WARN on a seed-time boot that can't reach
        # a not-yet-up peer). The reply is the trailing JSON object.
        brace = out.rfind("\n{")
        candidate = out[brace + 1 :] if brace != -1 else out[out.find("{") :]
        try:
            return json.loads(candidate)
        except (json.JSONDecodeError, ValueError) as e:
            raise RuntimeError(f"one-shot returned non-JSON: {out!r} ({e})") from e


def seed_update(
    binary: Launcher | Path, workdir: Path, agent_id: str, *, timeout: float = 15.0, **meta: Any
) -> dict[str, Any]:
    """One-shot `fantastic kernel update_agent id=<agent_id> k=v ...` — an
    OPERATOR write (ungated). Every named field is REPLACED, a dict field whole
    (`seed_grant` merges one edge into a grant map). Raises on a refusal."""
    args = ["kernel", "update_agent", f"id={agent_id}"]
    args += [f"{k}={_render(v)}" for k, v in meta.items()]
    reply = _oneshot(binary, workdir, args, timeout)
    if not isinstance(reply, dict) or reply.get("updated") is not True:
        raise RuntimeError(f"update_agent {agent_id} {sorted(meta)} refused: {reply}")
    return reply


def seed_grant(
    binary: Launcher | Path,
    workdir: Path,
    holder: str,
    field: str,
    grants: dict[str, Any],
    *,
    timeout: float = 15.0,
) -> None:
    """Add `grants` (`{key: "*" | [verbs]}`) to `holder`'s grant map `field`
    (`can_call` or `callable_by`): read the record (`list_agents`), merge, write the
    map back WHOLE — `update_agent` replaces a dict field, so this is how one edge
    is added without dropping the others."""
    listed = _oneshot(binary, workdir, ["kernel", "list_agents"], timeout)
    rec = next((a for a in listed.get("agents") or [] if a.get("id") == holder), None)
    if rec is None:
        raise RuntimeError(f"seed_grant: no agent {holder!r} in {listed}")
    merged = {**(rec.get(field) or {}), **grants}
    seed_update(binary, workdir, holder, timeout=timeout, **{field: merged})


def seed_store(binary: Path, workdir: Path) -> None:
    """Persist the persistence PROVIDER — a `file_bridge` rooted at `.fantastic`
    (id `store`, open). Must run BEFORE any other seed.

    Every runtime auto-persists records ONLY through a discovered
    `file_bridge@.fantastic`; with none wired the one-shot seeds stay in RAM and
    never reach disk, so the spawned daemon would boot empty. Seeding the store
    first (it self-persists, so it survives to the next one-shot) makes every later
    seed persist. The loader writes through it as kernel bookkeeping, so it needs no
    grant. Idempotent (skips if a `store` already exists)."""
    proc = as_launcher(binary).cli(workdir, ["reflect", "tree=ids"], timeout=15.0)
    if '"store"' in proc.stdout:
        return  # already wired
    reply = seed_create(
        binary,
        workdir,
        handler_module="file_bridge.tools",
        allow=["."],
        agent_id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    if "error" in reply:
        raise RuntimeError(f"store seed failed: {reply}")


def seed_memory(binary: Path, workdir: Path) -> None:
    """Persist the kernel's MEMORY STORE — a `ram_store` (id `memory`) under the
    root — and name it in the root's `memory_store_id`. Every runtime's daemon
    refuses to start without one (`kernel_start`: the kernel keeps its execution
    rows — ledger, dead letters, named-call dedup — in that store). The kernel
    writes its own rows as bookkeeping, so the store needs no grant. Idempotent."""
    proc = as_launcher(binary).cli(workdir, ["reflect", "tree=ids"], timeout=15.0)
    if f'"{MEMORY}"' in proc.stdout:
        return  # already wired
    reply = seed_create(binary, workdir, handler_module="ram_store.tools", agent_id=MEMORY)
    if "error" in reply:
        raise RuntimeError(f"memory store seed failed: {reply}")
    seed_update(binary, workdir, root_id(binary, workdir), memory_store_id=MEMORY)


def seed_web(binary: Path, workdir: Path, port: int) -> None:
    """Persist a `web` agent bound to `port` — call once per workdir.

    Ensures the persistence provider (`seed_store`) and the memory store
    (`seed_memory`) FIRST: the seeded tree must reach disk for the spawned daemon
    to load, and the daemon will not start without a memory store.

    `web` mounts its legs' routes by calling their `get_routes` AS ITSELF at boot,
    so it reaches its own subtree (`$subtree`); each leg accepts its `$parent`."""
    seed_store(binary, workdir)
    seed_memory(binary, workdir)
    reply = seed_create(
        binary,
        workdir,
        handler_module="web.tools",
        agent_id="web",
        port=port,
        can_call={**BASE, "$subtree": "*"},
    )
    if "error" in reply:
        raise RuntimeError(f"web seed failed: {reply}")


def seed_web_ws(binary: Path, workdir: Path, *, reach: dict[str, Any] | None = None) -> None:
    """Persist the `web_ws` leg (`DRIVER`) as a child of `web` — the surface every
    `KernelProc.call` rides (the host serves `/<id>/ws` only when a `web_ws` child
    contributes the route).

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all_ingress`): IO legs SEAL by
    default (an absent rule ⇒ `deny_ingress`). This is a drivable test surface —
    open it consciously; a test exercising a sealed leg sets the rule on the agent
    it cares about.

    The leg performs every frame AS ITSELF, so it is granted, both halves: it
    reaches the root (`*` — the kernel verbs `create_agent`/`update_agent`, reflect,
    the connect-time watch) and every id in `reach` (`{id: "*" | [verbs]}`); the
    root accepts it. A target in `reach` must name `DRIVER` in its own
    `callable_by` (set where that target is created)."""
    root = root_id(binary, workdir)
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_ws.tools",
        agent_id=DRIVER,
        parent_id="web",
        ingress_rule="allow_all_ingress",
        can_call={**BASE, root: "*", **(reach or {})},
        callable_by={**BASE, "$parent": "*"},
    )
    if "error" in reply:
        raise RuntimeError(f"web_ws seed failed: {reply}")
    seed_grant(binary, workdir, root, "callable_by", {DRIVER: "*"})


def seed_web_rest(binary: Path, workdir: Path, agent_id: str = "rest") -> str:
    """Persist a `web_rest` agent as a child of `web`. Contributes
    `POST /<self>/<target>` (verb in body) + `GET /<self>/_reflect`
    routes the host mounts at boot. Returns the agent id.

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all_ingress`) — a bare `web_rest`
    is sealed and answers every harness POST `403 {reason:"unauthorized"}`. Like
    `web_ws` it acts AS ITSELF: it reaches the root and the root accepts it."""
    root = root_id(binary, workdir)
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_rest.tools",
        agent_id=agent_id,
        parent_id="web",
        ingress_rule="allow_all_ingress",
        can_call={**BASE, root: "*"},
        callable_by={**BASE, "$parent": "*"},
    )
    if "error" in reply:
        raise RuntimeError(f"web_rest seed failed: {reply}")
    seed_grant(binary, workdir, root, "callable_by", {agent_id: "*"})
    return reply.get("id", agent_id)


# Per-runtime `handler_module` for the WS bridge agent. All three host runtimes
# now ship the WS derivation as `ws_bridge.tools` (they split the combined
# kernel_bridge into ws_bridge + relay_connector). Keyed by the RUNTIME enum from
# the root reflect (the bridge is seeded on the DIALING kernel, so it matches
# THAT runtime).
_WS_BRIDGE_HANDLER_MODULE = {
    "python": "ws_bridge.tools",
    "rust": "ws_bridge.tools",
    "swift": "ws_bridge.tools",
}


def seed_bridge_ws(
    binary: Path,
    workdir: Path,
    *,
    agent_id: str,
    peer_id: str,
    peer_port: int,
    host: str = "127.0.0.1",
) -> dict[str, Any]:
    """Persist a WS-transport bridge agent (asymmetric — no peer bridge
    needed) on the DIALING kernel.

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

    The `handler_module` is resolved per the dialing runtime
    (`_WS_BRIDGE_HANDLER_MODULE`): `ws_bridge.tools` on Python + Rust,
    `kernel_bridge.tools` on Swift (its port is deferred).
    """
    handler_module = _WS_BRIDGE_HANDLER_MODULE.get(
        runtime(binary, workdir) or "", "kernel_bridge.tools"
    )
    reply = seed_create(
        binary,
        workdir,
        handler_module=handler_module,
        agent_id=agent_id,
        transport="ws",
        peer_id=peer_id,
        port=peer_port,
        host=host,
    )
    if "error" in reply:
        raise RuntimeError(f"ws bridge seed failed: {reply}")
    return reply
