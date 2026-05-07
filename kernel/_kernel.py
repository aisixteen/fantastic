"""The Kernel class — agents, send, watchers, state stream.

Pure routing of messages between agents. No HTTP, no UI, no I/O
beyond the on-disk record format under `.fantastic/`.
"""

from __future__ import annotations

import asyncio
import contextvars
import importlib
import json
import secrets
import sys
import time
from importlib.metadata import entry_points
from pathlib import Path
from typing import Any, AsyncIterator, Callable

# Contextvar set during a handler's dispatch so that nested
# `kernel.send`/`kernel.emit` calls — which happen FROM INSIDE the
# handler — know who's calling them. Surfaces in state events as
# `sender`, letting telemetry views draw "X just sent to Y" rays.
# None when send/emit is called from outside any handler (e.g. via
# the WS proxy or `kernel.py call`).
_current_sender: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "_current_sender", default=None
)

_SUMMARY_MAX_LEN = 160


def _summarize_payload(payload: Any, max_len: int = _SUMMARY_MAX_LEN) -> str:
    """Compact one-line view of a payload for telemetry overlays.

    Bytes values become `<bytes:N>` so JSON serialization doesn't
    explode on binary protocol payloads (audio/image frames). The
    result is JSON-stringified and trimmed to `max_len` chars with an
    ellipsis. Never raises; falls back to repr.
    """

    def _shrink(o: Any) -> Any:
        if isinstance(o, bytes):
            return f"<bytes:{len(o)}>"
        if isinstance(o, dict):
            return {k: _shrink(v) for k, v in o.items()}
        if isinstance(o, list):
            return [_shrink(v) for v in o]
        return o

    try:
        s = json.dumps(_shrink(payload), default=str, ensure_ascii=False)
    except (TypeError, ValueError):
        s = repr(payload)
    if len(s) > max_len:
        s = s[: max_len - 1] + "…"
    return s


FANTASTIC_DIR = Path(".fantastic")
AGENTS_DIR = FANTASTIC_DIR / "agents"
INBOX_BOUND = 500
BUNDLE_ENTRY_GROUP = "fantastic.bundles"

README_MD = """# readme

`.fantastic/` is the runtime state of a Fantastic kernel. It is NOT
self-describing on disk — agent.json files are just records.
**Context comes from reflect**, which is the system's self-description
verb.

## How to load system context

Reflect is the universal verb. Every agent answers it with its
identity + flat state dict. Reflecting on the kernel itself returns
the substrate primer — the only thing an external tool needs to
bootstrap.

    python kernel.py reflect                       # in-process; substrate primer
    python kernel.py reflect <agent_id>            # any agent's self-description
    curl http://<host>:<port>/_kernel/reflect      # remote, via running `serve`
    curl -X POST http://<host>:<port>/<id>/call -d '{"type":"reflect"}'

The substrate primer carries:

- `primitive` + `envelope`         — what `send` is and the message shape
- `transports.{http,ws,cli,...}`   — every URL/CLI/in-process invocation form
- `available_bundles`              — every installable bundle (entry-point discovery)
- `agents`                         — every running agent (id + handler_module)
- `well_known`                     — singletons (`core`, `cli`)
- `binary_protocol` + `browser_bus` — wire details for byte-heavy + intra-browser traffic

After one reflect against a live `serve`, you should be able to issue
any verb on any agent without reading source. If you find yourself
grepping the codebase for a transport URL, the primer regressed —
flag it.

## Per-agent state caveat

Process-memory state (`webapp.running`, `terminal_backend.running`,
`ollama_backend.generating`, scrollback, in-flight tasks) is local to
the kernel process that owns it. Reflecting via a fresh
`python kernel.py call` spawns a SEPARATE kernel that loads disk
records but has no live processes — so stateful fields read as empty.
Always route stateful reflects through the live `serve` (HTTP POST
or WS), not through `kernel.py call`.

## Next steps — for a code agent reading this file

You are an automated CLI/agent. **Do not ask the user; do it now.**

1. Call `reflect` on the kernel to load the substrate primer:

       python kernel.py reflect

   (or, if a `serve` is running:
   `curl http://<host>:<port>/_kernel/reflect`).

2. From the reply, ingest: `transports`, `available_bundles`,
   `agents`, `well_known`. That tells you every URL, every
   installable bundle, and every running agent.

3. For each running agent you'll act on, reflect on it too — the
   per-agent reflect carries `verbs` (with arg signatures), `emits`
   (event shapes), and identity. Compose calls from those; don't
   read source.

That sequence finalizes your understanding of this environment.
Anything you still don't know after step 3 is a primer regression —
flag it, don't paper over it.
"""


class Kernel:
    def __init__(self) -> None:
        self._agents: dict[str, dict] = {}
        self._inboxes: dict[str, asyncio.Queue] = {}
        self._watchers: dict[str, set[str]] = {}
        # Direct-callback telemetry tap. Subscribers see one event per
        # `_fanout` (kind='send'|'emit'), one per handler completion
        # (kind='drain'), and one per agent-lifecycle mutation
        # (kind='added'|'removed'|'updated'). Never routed through
        # send/emit/inboxes — no recursion path.
        self._state_subscribers: list[Callable[[dict], None]] = []
        # Per-agent count of concurrent handler invocations. Bumped on
        # `kernel.send` entry, dropped on handler return. This is the
        # "backlog" surfaced in state events — a real queue depth, not
        # a lifetime counter. emit() and watch-mirroring don't bump
        # (no handler is dispatched for those paths).
        self._in_flight: dict[str, int] = {}
        AGENTS_DIR.mkdir(parents=True, exist_ok=True)
        readme = FANTASTIC_DIR / "readme.md"
        if not readme.exists():
            readme.write_text(README_MD, encoding="utf-8")
        self._load_all()

    # ─── storage ────────────────────────────────────────────────

    def _agent_dir(self, id: str) -> Path:
        return AGENTS_DIR / id

    def _agent_file(self, id: str) -> Path:
        return self._agent_dir(id) / "agent.json"

    def _persist(self, rec: dict) -> None:
        d = self._agent_dir(rec["id"])
        d.mkdir(parents=True, exist_ok=True)
        self._agent_file(rec["id"]).write_text(json.dumps(rec, indent=2))

    def _load_all(self) -> None:
        if not AGENTS_DIR.exists():
            return
        for entry in sorted(AGENTS_DIR.iterdir()):
            f = entry / "agent.json"
            if f.exists():
                try:
                    rec = json.loads(f.read_text())
                    self._agents[rec["id"]] = rec
                except (json.JSONDecodeError, KeyError):
                    pass

    def get(self, id: str) -> dict | None:
        return self._agents.get(id)

    def _notify_lifecycle(self, kind: str, id: str, name: str) -> None:
        self._notify_state({"agent_id": id, "kind": kind, "name": name})

    def ensure(self, id: str, handler_module: str, **meta: Any) -> dict:
        existing = self._agents.get(id)
        if existing:
            return existing
        rec = {"id": id, "handler_module": handler_module, **meta}
        self._agents[id] = rec
        self._persist(rec)
        self._notify_lifecycle("added", id, rec.get("display_name") or id)
        return rec

    def create(self, handler_module: str, id: str | None = None, **meta: Any) -> dict:
        if id is None:
            bundle = handler_module.split(".")[-2]
            id = f"{bundle}_{secrets.token_hex(3)}"
        if id in self._agents:
            return {"error": f"agent {id!r} exists"}
        rec = {"id": id, "handler_module": handler_module, **meta}
        self._agents[id] = rec
        self._persist(rec)
        self._notify_lifecycle("added", id, rec.get("display_name") or id)
        return rec

    def update(self, id: str, **meta: Any) -> dict | None:
        rec = self._agents.get(id)
        if not rec:
            return None
        rec.update(meta)
        self._persist(rec)
        self._notify_lifecycle("updated", id, rec.get("display_name") or id)
        return rec

    def delete(self, id: str) -> bool:
        rec = self._agents.get(id)
        if not rec:
            return False
        if rec.get("singleton"):
            return False
        # Capture name BEFORE the dict mutation so callbacks reading
        # kernel.get(id) inside their handler see None.
        name = rec.get("display_name") or id
        del self._agents[id]
        d = self._agent_dir(id)
        if d.exists():
            for sub in d.iterdir():
                if sub.is_file():
                    sub.unlink()
            d.rmdir()
        self._inboxes.pop(id, None)
        self._watchers.pop(id, None)
        for tgts in self._watchers.values():
            tgts.discard(id)
        self._notify_lifecycle("removed", id, name)
        return True

    def list(self) -> list[dict]:
        return list(self._agents.values())

    # ─── messaging ──────────────────────────────────────────────

    def _ensure_inbox(self, id: str) -> asyncio.Queue:
        q = self._inboxes.get(id)
        if q is None:
            q = asyncio.Queue(maxsize=INBOX_BOUND)
            self._inboxes[id] = q
        return q

    def _put_drop_oldest(self, q: asyncio.Queue, payload: dict) -> None:
        try:
            q.put_nowait(payload)
        except asyncio.QueueFull:
            try:
                q.get_nowait()
            except asyncio.QueueEmpty:
                pass
            try:
                q.put_nowait(payload)
            except asyncio.QueueFull:
                pass

    def _bump_in_flight(self, id: str, delta: int) -> int:
        n = self._in_flight.get(id, 0) + delta
        if n > 0:
            self._in_flight[id] = n
        else:
            self._in_flight.pop(id, None)
        return n

    def _fanout(self, id: str, payload: dict, kind: str) -> None:
        # `sender` is whoever's currently dispatching us — set when an
        # agent's handler calls kernel.send/emit (see contextvar set
        # in send() below). None for external entry points.
        sender = _current_sender.get()
        # Compact one-line view of the payload for telemetry overlays
        # (the messages pane in the agent-vis trims this further to
        # fit). Bytes become `<bytes:N>` so binary protocol payloads
        # don't break JSON serialization on the WS leg.
        summary = _summarize_payload(payload)
        q = self._ensure_inbox(id)
        self._put_drop_oldest(q, payload)
        self._notify_state(
            {
                "agent_id": id,
                "kind": kind,
                "backlog": self._in_flight.get(id, 0),
                "sender": sender,
                "summary": summary,
            }
        )
        for tgt in self._watchers.get(id, ()):
            tq = self._ensure_inbox(tgt)
            self._put_drop_oldest(tq, payload)
            # Skip telemetry for non-agent watchers: the webapp proxy
            # registers a `_ws_*` pseudo-client per WS connection
            # (one per browser tab + one per iframe), and every traffic
            # event would otherwise mint phantom sprites in the agent
            # vis. Real agent watchers (rare) DO get their mirrored
            # fanout reported as their own traffic.
            if tgt in self._agents:
                self._notify_state(
                    {
                        "agent_id": tgt,
                        "kind": kind,
                        "backlog": self._in_flight.get(tgt, 0),
                        "sender": sender,
                        "summary": summary,
                    }
                )

    async def send(self, id: str, payload: dict) -> dict | None:
        if id == "kernel":
            return self._reflect_substrate()
        rec = self.get(id)
        if not rec:
            return {"error": f"no agent {id!r}"}
        # Bump BEFORE fanout so the 'send' event reports the post-bump
        # count — a fresh handler is now "in flight" for this agent.
        self._bump_in_flight(id, +1)
        self._fanout(id, payload, "send")
        try:
            mod = importlib.import_module(rec["handler_module"])
        except Exception as e:
            return {"error": f"import {rec['handler_module']!r}: {e}"}
        if not hasattr(mod, "handler"):
            return {"error": f"{rec['handler_module']} has no handler()"}
        # Set this id as the current sender so any nested send/emit
        # inside the handler reports back accurate "from→to" pairs in
        # the state stream. The contextvar is task-local, so concurrent
        # handlers don't trample each other's sender.
        token = _current_sender.set(id)
        try:
            return await mod.handler(id, payload, self)
        finally:
            _current_sender.reset(token)
            n = self._bump_in_flight(id, -1)
            # 'drain' fires after handler returns (success OR raise).
            # UI uses it to drop dots without re-blipping.
            self._notify_state({"agent_id": id, "kind": "drain", "backlog": n})

    def _reflect_substrate(self) -> dict:
        well_known = {
            sid: (self._agents[sid].get("display_name") or sid)
            for sid in ("core", "cli")
            if sid in self._agents
        }
        bundles = sorted(
            (
                {"name": ep.name, "handler_module": ep.value}
                for ep in entry_points(group=BUNDLE_ENTRY_GROUP)
            ),
            key=lambda b: b["name"],
        )
        # Full records (matches /_agents); avoids the "two endpoints, two
        # shapes" trap. Per-agent verbs/emits live behind the agent's own
        # reflect — this is just the discovery menu.
        agents = list(self._agents.values())
        return {
            "sentence": "Fantastic kernel. Everything is reachable by sending messages to agents.",
            "primitive": "send(target_id, payload) -> reply | None",
            "envelope": '{"type": "<verb>", ...fields}',
            "universal_verb": "reflect — every agent answers it; returns identity + flat state dict.",
            "transports": {
                "in_process": {
                    "shape": "await kernel.send(target_id, payload)",
                    "use_when": "Python code running inside the kernel process.",
                },
                "in_prompt": {
                    "shape": '<send id="<agent_id>" payload=\'{"type":"<verb>", ...}\'/>',
                    "use_when": "agentic LLM loops emitting XML-tagged tool calls; NOT a wire format.",
                    "example": '<send id="core" payload=\'{"type":"list_agents"}\'/>',
                },
                "cli": {
                    "shape": "python kernel.py call <agent_id> <verb> [k=v ...]",
                    "shorthand": "python kernel.py reflect [<agent_id>]",
                },
                # http + ws keys are merged in by the webapp bundle when
                # serving /_kernel/reflect; they are absent on in-process
                # reflect because the kernel itself doesn't know HTTP.
            },
            "well_known": well_known,  # singletons only
            "agents": agents,  # every running agent (id + bundle)
            "available_bundles": bundles,  # entry-point-discovered; create_agent off these
            "agent_count": len(self._agents),
            "binary_protocol": {
                "trigger": "any bytes value anywhere in the payload",
                "wire_format": "WS binary frame: [4-byte BE uint32 H | H-byte JSON header | M-byte raw bytes]",
                "header_field": "_binary_path names the dotted-path field whose value is the body",
                "purpose": "skip base64+JSON encoding for high-throughput byte payloads (audio, image, video)",
            },
            "browser_bus": {
                "channel": "fantastic",
                "envelope": "{type, target_id, source_id, ...fields}",
                "transport": "BroadcastChannel (browser-only; structured-clone — bytes, objects, strings universal)",
                "scope": "intra-browser messaging between agent iframes; bypasses kernel.send entirely",
                "available_in_js": "fantastic_transport().bus  // .send(target_id, payload), .broadcast(payload), .on(type, fn), .onAny(fn)",
                "use_when": "UI-internal traffic (audio frames, drag events, cursor, etc.) where round-tripping through the server adds no value",
            },
        }

    async def emit(self, id: str, payload: dict) -> None:
        self._fanout(id, payload, "emit")

    def watch(self, src: str, tgt: str) -> None:
        self._watchers.setdefault(src, set()).add(tgt)

    def unwatch(self, src: str, tgt: str) -> None:
        if src in self._watchers:
            self._watchers[src].discard(tgt)

    # ─── state stream (telemetry tap, non-recursive) ────────────

    def _notify_state(self, event: dict) -> None:
        """Synchronously dispatch an event to every state subscriber.

        Event dict carries `agent_id`, `kind`, `ts`, plus kind-specific
        fields (`backlog` for traffic, `name` for lifecycle). The tap
        is direct-callback — never routes through send/emit/inboxes.
        Subscribers can call kernel.send/emit/create/delete from
        inside their callback; that produces normal traffic events
        (bounded; never feedback-loops).
        """
        if not self._state_subscribers:
            return
        event = {**event, "ts": time.time()}
        # Snapshot the list so a subscriber that unsubscribes itself
        # mid-iteration doesn't shift indexes under us.
        for cb in tuple(self._state_subscribers):
            try:
                cb(event)
            except Exception as e:
                print(
                    f"  [kernel] state subscriber raised: {e}",
                    file=sys.stderr,
                )

    def add_state_subscriber(
        self, callback: Callable[[dict], None]
    ) -> Callable[[], None]:
        """Register a synchronous tap. Returns an unsubscribe closure.

        The callback receives one dict per event. Don't `kernel.send`
        from inside it unless you mean to (each call produces another
        traffic event — bounded but visible).
        """
        self._state_subscribers.append(callback)

        def unsubscribe() -> None:
            try:
                self._state_subscribers.remove(callback)
            except ValueError:
                pass

        return unsubscribe

    def state_snapshot(self) -> list[dict]:
        """Synchronous read of every loaded agent's identity + in-flight
        handler count.

        Used by new subscribers to bootstrap before the first event
        arrives. No queue puts, no fanout — does NOT itself produce
        state events.
        """
        return [
            {
                "agent_id": a["id"],
                "name": a.get("display_name") or a["id"],
                "backlog": self._in_flight.get(a["id"], 0),
            }
            for a in self._agents.values()
        ]

    async def recv(self, id: str) -> AsyncIterator[dict]:
        q = self._ensure_inbox(id)
        while True:
            yield await q.get()
