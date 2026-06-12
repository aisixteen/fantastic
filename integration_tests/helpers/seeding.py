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
import re
import zipfile
from pathlib import Path
from typing import Any

from .launcher import as_launcher

_ROOT_ID_CACHE: dict[str, str] = {}
_RUNTIME_CACHE: dict[str, str | None] = {}

# Repo root: integration_tests/helpers -> integration_tests -> repo.
_REPO_ROOT = Path(__file__).resolve().parents[2]

# Sovereign frontend artifact built by `cd ts && sh scripts/pack.sh`. Holds
# exactly three entries: readme.md, bundle.min.js, bundle.min.js.map.
FRONTEND_ZIP = _REPO_ROOT / "ts" / "dist" / "js_kernel.zip"
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


def root_id(binary: Path, workdir: Path, *, timeout: float = 15.0) -> str:
    """Discover the runtime's ROOT agent id via a one-shot `reflect`.

    Python's root is `kernel_state`; rust/swift use `core`. The harness must
    not hardcode either — seeds (and WS paths / reflect targets) that
    attach to the root resolve it here. Cached per (binary, workdir).

    `binary` may be a launcher (local or container) or a raw binary path —
    `as_launcher` coerces a path so legacy call sites keep working.
    """
    key = f"{binary}|{workdir}"
    if key not in _ROOT_ID_CACHE:
        proc = as_launcher(binary).cli(workdir, ["reflect"], timeout=timeout)
        out = proc.stdout
        rid, rt = "core", None
        brace = out.find("{")
        if brace != -1:
            try:
                obj = json.loads(out[brace:])
                rid = obj.get("id") or "core"
                rt = obj.get("runtime")
            except (json.JSONDecodeError, ValueError):
                pass
        _ROOT_ID_CACHE[key] = rid
        _RUNTIME_CACHE[key] = rt
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
    """One-shot `fantastic <parent_id> create_agent ...` against
    `workdir`. Dispatch is intentionally sent to the PARENT agent
    (not always `core`) because Python's substrate places new
    children under the calling agent's children dir, regardless of
    a `parent_id=` field in the payload. To get
    `.fantastic/agents/web/agents/web_ws/agent.json` (nested), we
    must call `fantastic web create_agent ...`.
    """
    if parent_id is None:
        parent_id = root_id(binary, workdir, timeout=timeout)
    args: list[str] = [
        parent_id,
        "create_agent",
        f"handler_module={handler_module}",
    ]
    if agent_id is not None:
        args.append(f"id={agent_id}")
    for k, v in meta.items():
        args.append(f"{k}={_render(v)}")

    proc = as_launcher(binary).cli(workdir, args, timeout=timeout)
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
            raise RuntimeError(f"seed_create returned non-JSON: {out!r} ({e})") from e


def _render(v: Any) -> str:
    """Render a Python value for the CLI's `k=v` format. Bool → true/false,
    int/float → string, str → bare string. Matches Python's `_coerce`
    semantics in reverse."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    return str(v)


def seed_store(binary: Path, workdir: Path) -> None:
    """Persist the persistence PROVIDER — a `file_bridge` rooted at `.fantastic`
    (id `store`, open). Must run BEFORE any other seed.

    PYTHON + RUST + SWIFT. Under the no-fallback rule, every kernel auto-persists
    records ONLY through a discovered `file_bridge@.fantastic`; with none wired the
    one-shot seeds stay in RAM and never reach disk, so the spawned daemon would boot
    empty. Seeding the store first (it self-persists, so it survives to the next
    one-shot) makes every later seed persist. Idempotent (skips if a `store`
    already exists)."""
    proc = as_launcher(binary).cli(workdir, ["reflect", "tree=ids"], timeout=15.0)
    if '"store"' in proc.stdout:
        return  # already wired
    reply = seed_create(
        binary,
        workdir,
        handler_module="file_bridge.tools",
        agent_id="store",
        root=".fantastic",
        ingress_rule="allow_all",
    )
    if "error" in reply:
        raise RuntimeError(f"store seed failed: {reply}")


def seed_web(binary: Path, workdir: Path, port: int) -> None:
    """Persist a `web` agent bound to `port`. Idempotent if the
    runtime's one-shot CLI handles "agent already exists" gracefully;
    otherwise call only once per workdir.

    Ensures the persistence provider (`seed_store`) FIRST, so the seeded tree
    actually reaches disk for the spawned daemon to load (python no-fallback rule).
    """
    seed_store(binary, workdir)
    reply = seed_create(
        binary,
        workdir,
        handler_module="web.tools",
        agent_id="web",
        port=port,
    )
    if "error" in reply:
        raise RuntimeError(f"web seed failed: {reply}")


def seed_web_ws(binary: Path, workdir: Path) -> None:
    """Persist a `web_ws` agent as a child of `web`. Required on BOTH
    kernels for WS — the host serves `/<id>/ws` only when a `web_ws`
    child contributes the route (WS is opt-in, parity with Python).

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all`). IO legs now SEAL
    by default (an absent rule ⇒ `deny_inbound`), so a bare `web_ws` would
    refuse every forwarded reflect/call the harness dispatches through it.
    This is a drivable test surface — open it consciously. A test that
    means to exercise a SEALED or credentialed leg sets its own rule
    explicitly on the agent it cares about (not on this serving surface)."""
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_ws.tools",
        agent_id="web_ws",
        parent_id="web",
        ingress_rule="allow_all",
    )
    if "error" in reply and "no bundle" not in str(reply.get("error", "")):
        raise RuntimeError(f"web_ws seed failed: {reply}")


def seed_web_rest(binary: Path, workdir: Path, agent_id: str = "rest") -> str:
    """Persist a `web_rest` agent as a child of `web`. Contributes
    `POST /<self>/<target>` (verb in body) + `GET /<self>/_reflect`
    routes the host mounts at boot. Returns the agent id. Works on
    both kernels (web_rest.tools is registered in each).

    Seeded EXPLICITLY OPEN (`ingress_rule=allow_all`). IO legs now SEAL
    by default (an absent rule ⇒ `deny_inbound`), so a bare `web_rest`
    would answer every harness POST with `403 {reason:"unauthorized"}`.
    This is a drivable diagnostic surface — open it consciously."""
    reply = seed_create(
        binary,
        workdir,
        handler_module="web_rest.tools",
        agent_id=agent_id,
        parent_id="web",
        ingress_rule="allow_all",
    )
    if "error" in reply and "no bundle" not in str(reply.get("error", "")):
        raise RuntimeError(f"web_rest seed failed: {reply}")
    return reply.get("id", agent_id)


# Per-runtime `handler_module` for the WS bridge agent. All three host runtimes
# now ship the WS derivation as `ws_bridge.tools` (they split the combined
# kernel_bridge into ws_bridge + cloud_bridge). Keyed by the RUNTIME enum from
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

    Note: `local_port` is the historical field name for "the port to
    dial" (shared with the ssh+ws tunnel path); here it carries
    `peer_port`.

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
        local_port=peer_port,
        host=host,
    )
    if "error" in reply:
        raise RuntimeError(f"ws bridge seed failed: {reply}")
    return reply
