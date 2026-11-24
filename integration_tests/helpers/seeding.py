"""Workdir seeding for fantastic kernels — via one-shot CLI calls.

Every host runtime's CLI runs `fantastic_kernel <id> <verb> [k=v ...]` as a
one-shot OPERATOR call against the workdir (no daemon up; the operator is
ungated). We use that to create agents and write their lines (`can`), so the on-disk
format is whatever the runtime itself writes (`.fantastic/agent.yaml` for the
root, nested `agents/<id>/agents/<child>/agent.yaml` below it). This avoids
hand-rolling records that might diverge from the canonical shape, and it runs
inside the container under `FANTASTIC_TARGET=container` (records owned by the
container's uid).

Sealed by default: a fresh agent holds no line (`can` absent or `{}`) and
reaches only its own tree; an agent with no line covering a target gets the
unknown answer (`no agent '<id>'`), the same bytes as a missing id. The caller's
`can` alone decides, capped by every ancestor whose tree does not hold the
target; the target keeps no list. The seeds below write exactly the lines the
harness drives, on the caller and on the cap.

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
    """Discover the runtime enum (`python`/`rust`/`ts`) from the root
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


#: Every verb of a target: the line an old `"*"` grant became.
ALL: dict[str, Any] = {"call": ["*"]}


def call(*verbs: str) -> dict[str, Any]:
    """A `call` line naming `verbs`."""
    return {"call": list(verbs)}


#: The WS leg every `KernelProc.call` rides (`seed_web_ws`). One leg is one
#: principal: every frame the harness sends is performed AS this agent, so its
#: own `can` (and `web`'s, the cap) must cover each target it drives.
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
    (`seed_grant` merges one line into `can`). Raises on a refusal."""
    args = ["kernel", "update_agent", f"id={agent_id}"]
    args += [f"{k}={_render(v)}" for k, v in meta.items()]
    reply = _oneshot(binary, workdir, args, timeout)
    if not isinstance(reply, dict) or reply.get("updated") is not True:
        raise RuntimeError(f"update_agent {agent_id} {sorted(meta)} refused: {reply}")
    return reply


def seed_reflect(
    binary: Launcher | Path, workdir: Path, agent_id: str = "kernel", *, timeout: float = 15.0, **args: Any
) -> dict[str, Any]:
    """One-shot `fantastic <agent_id> reflect k=v ...` — an OPERATOR read (no
    daemon up): the whole reply, `can` and every field included."""
    reply = _oneshot(binary, workdir, [agent_id, "reflect", *(f"{k}={_render(v)}" for k, v in args.items())], timeout)
    if not isinstance(reply, dict):
        raise RuntimeError(f"reflect {agent_id}: not an object: {reply!r}")
    return reply


def seed_grant(
    binary: Launcher | Path,
    workdir: Path,
    holder: str,
    line_target: str,
    line: dict[str, Any],
    *,
    timeout: float = 15.0,
) -> None:
    """Give `holder` the line `line` (`{call: [...]}`, `{update: [...]}` or
    `{create: {who, what}}`) on `line_target`: read the record (operator
    `reflect`), set `can[line_target]`, write `can` back WHOLE — `update_agent`
    replaces a dict field, so this is how one line is added without dropping the
    others. The operator may name an id that does not exist yet."""
    rec = seed_reflect(binary, workdir, holder, timeout=timeout)
    if rec.get("id") != holder:
        raise RuntimeError(f"seed_grant: no agent {holder!r}: {rec}")
    merged = {**(rec.get("can") or {}), line_target: line}
    seed_update(binary, workdir, holder, timeout=timeout, can=merged)


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

    `web` holds no line: it mounts its legs' routes by calling their
    `get_routes` AS ITSELF at boot, and its legs are in its own tree (position).
    A line `web` needs on an agent outside its tree (the cap on a leg's line, a
    proxied agent) is added with `seed_grant`."""
    seed_store(binary, workdir)
    seed_memory(binary, workdir)
    reply = seed_create(
        binary,
        workdir,
        handler_module="web.tools",
        agent_id="web",
        port=port,
    )
    if "error" in reply:
        raise RuntimeError(f"web seed failed: {reply}")


def _leg_lines(binary: Launcher | Path, workdir: Path, reach: dict[str, Any] | None) -> None:
    """The cap on a leg's lines: `web` (the leg's parent) gets the same line on
    every target of `reach` outside its own tree. `web` itself is in its tree."""
    for target, line in (reach or {}).items():
        if target != "web":
            seed_grant(binary, workdir, "web", target, line)


def seed_web_ws(binary: Path, workdir: Path, *, reach: dict[str, Any] | None = None) -> None:
    """Persist the `web_ws` leg (`DRIVER`) as a child of `web` — the surface every
    `KernelProc.call` rides (the host serves `/<id>/ws` only when a `web_ws` child
    contributes the route).

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all_ingress`): IO legs SEAL by
    default (an absent rule ⇒ `deny_ingress`). This is a drivable test surface —
    open it consciously; a test exercising a sealed leg sets the rule on the agent
    it cares about.

    The leg performs every frame AS ITSELF and holds exactly the lines of `reach`
    (`{id: line}`, e.g. `{"js_kernel": ALL}`); `web` gets the same lines (the cap:
    those targets are outside web's tree). Nothing is written on the root or on
    any target. With no `reach` the leg reaches only its own tree."""
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_ws.tools",
        agent_id=DRIVER,
        parent_id="web",
        ingress_rule="allow_all_ingress",
        **({"can": dict(reach)} if reach else {}),
    )
    if "error" in reply:
        raise RuntimeError(f"web_ws seed failed: {reply}")
    _leg_lines(binary, workdir, reach)


def seed_web_rest(
    binary: Path, workdir: Path, agent_id: str = "rest", *, reach: dict[str, Any] | None = None
) -> str:
    """Persist a `web_rest` agent as a child of `web`. Contributes
    `POST /<self>/<target>` (verb in body) + `GET /<self>/_reflect`
    routes the host mounts at boot. Returns the agent id.

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all_ingress`) — a bare `web_rest`
    is sealed and answers every harness POST `403 {reason:"unauthorized"}`. Like
    `web_ws` it acts AS ITSELF and holds exactly the lines of `reach`; `web` gets
    the same lines (the cap)."""
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_rest.tools",
        agent_id=agent_id,
        parent_id="web",
        ingress_rule="allow_all_ingress",
        **({"can": dict(reach)} if reach else {}),
    )
    if "error" in reply:
        raise RuntimeError(f"web_rest seed failed: {reply}")
    _leg_lines(binary, workdir, reach)
    return reply.get("id", agent_id)


# Per-runtime `handler_module` for the WS bridge agent. All three host runtimes
# now ship the WS derivation as `ws_bridge.tools` (they split the combined
# kernel_bridge into ws_bridge + relay_connector). Keyed by the RUNTIME enum from
# the root reflect (the bridge is seeded on the DIALING kernel, so it matches
# THAT runtime).
_WS_BRIDGE_HANDLER_MODULE = {
    "python": "ws_bridge.tools",
    "rust": "ws_bridge.tools",
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
    peer's `web_ws` dispatches
    `kernel.send(target, payload)` and replies over the same socket.
    No B-side bridge agent, no read_loop on the remote.

    The `handler_module` is resolved per the dialing runtime
    (`_WS_BRIDGE_HANDLER_MODULE`): `ws_bridge.tools` on Python + Rust.
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
