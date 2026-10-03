# fantastic-kernel

A medium that unifies humans and AIs into a single workspace.
Plugin-discovered agents, one primitive (`send`), hermetic protocol.

**Emergent, self-described wiring.** Every agent is at once an *actor* and a
`reflect`-able *description of itself*, and reaches any peer through one primitive
(`send`) that the kernel routes local-or-remote. So an LLM can introspect the live
system from its own readme + `reflect` surface and *compose* it — weaving agents
across the federated two-kernel topology (this host Python kernel ⇄ a browser JS
frontend kernel) into **durable interactive wiring** (e.g. an HTML panel whose
button runs background Python and pushes the result to a sibling panel) that humans
then actuate with no model in the loop. Capabilities and topologies are emergent
compositions read out of the substrate's own self-account — not features engineered
into it; the running artifact is a function of its *descriptance*, not its code.

## Concept

- **Agent** — recursive node, the universal type. Every entity in the
  system is an `Agent`. Agents have a persistent record on disk
  (`agent.yaml`), an asyncio inbox, and a `_children` dict that's
  empty for leaves and populated for hosts. Same code at every depth.
  Each agent answers `{type:"reflect"}` returning a flat self-description.
- **Kernel** — tree-wide shared context object. NOT an agent. NOT a
  base class. Constructed explicitly: `kernel = Kernel()`. Holds
  `agents: dict[id, Agent]` (flat routing index — derived from the
  tree, never written externally), `root: Agent` (the tree root),
  state subscribers, bundle resolver cache, well-known names. Exposes
  `kernel.create/delete/update/list/get/send` for tree management.
- **`kernel_state`** — the tree root IS an `kernel_state` agent
  (`id="kernel_state"`, `handler_module="kernel_state.tools"`): the
  persistence/hydration root that owns `.fantastic/`. A fresh dir
  seeds `{id:"kernel_state", handler_module:"kernel_state.tools"}`. The
  bootstrap composes a stdout renderer child when stdin is a tty.
- **memory store** — the kernel keeps NO conversation: every message is a
  row in a MEMORY STORE agent (`ram_store` ships here — in RAM, gone with the
  process; a durable store answers the same verbs). The root names it in
  `memory_store_id`, and `kernel_start` refuses to run without one; the kernel
  flushes its execution rows (`call`/`settle`/`ack`/`flush`) there, and
  `ledger`, dead letters (`on_deadletter`, `ack_deadletter`) and named-call
  dedup read them back. Each AI agent names its own store for its
  conversation; context moves between agents only by GRANT (`share` in the
  store → `fork_from` on the reader). Lifecycle verbs: `kernel_start`,
  `shutdown_agents`, `kernel_stop`. See `reflect kernel readme=memory`.
- **send** — the one primitive. `kernel.send(target_id, payload) →
  reply | None` (from outside any handler) or `agent.send(...)`
  (from inside) resolves the target via the flat index and
  dispatches.
- **streams** — `send` returns ONE JSON reply and `emit` fires ONE event;
  for BYTES that don't fit a single message (a file, an image, a live feed)
  agents speak a chunked **stream** protocol over the binary channel (raw
  bytes both ways, never base64) — a PULL with a cursor + backpressure,
  addressed by id, NOT the event system. Three duck-typed verbs:
  `read_stream {path, offset?} → ({path, offset, next_offset, eof, size},
  bytes)` (the **SOURCE** — pull one chunk, advance the stateless cursor by
  `next_offset` until `eof`); `write_stream {path, offset?, truncate?}` +
  body `bytes → {written, offset, size}` (the **SINK** — push one chunk);
  `pump {source, source_path, sink, sink_path?, chunk?} → {bytes, chunks}`
  (the **PUMP** — a server-side SOURCE→SINK copy that only coordinates and
  never touches the bytes, so fs→fs, `network_bridge`→`file_bridge`,
  anywhere→anywhere copy identically; each end self-gates/clamps, and both run
  AS THE PUMP'S CALLER — its own grants on both ends). A consumer
  is storage-agnostic — it pulls/pushes by id. `file_bridge` is the reference
  SOURCE+SINK; the served `/<id>/file/<path>` octet route is read_stream-only.
- **Cascade delete** — `delete_agent` walks the subtree depth-first
  via `_children`. Each descendant runs its `on_delete` hook (kills
  PTY / drains uvicorn / closes clients — PROCESS state only)
  BEFORE detaching from `kernel.agents` and the parent's
  `_children`. DISK cleanup is decoupled: the `removed` state event
  drives the loader to rmtree the dir, NOT `on_delete`. Any
  `delete_lock` anywhere in the subtree blocks the entire cascade
  with `{locked, blocked_by, error}` — no partial mutations.
- **Persistence** — DECOUPLED from `Agent`, which never touches disk.
  A loader agent (`kernel_state`) owns the medium, driven by the kernel
  STATE STREAM (debounced flush). The kernel exposes `save()`/`load()`
  over a flat record list (weak-load of unknown bundles). `main.py`
  bootstraps `Kernel()` → `kernel_state.read_tree('.fantastic')` →
  `kernel.load(records)`, rehydrating the whole tree by ids and
  respecting parent-child links. Process-memory state (in-flight
  counters, inboxes, PTY children) does NOT survive; bundles' `_boot`
  respawns it. Agents with class-level `ephemeral=True` (e.g. the cli
  renderer) are never persisted — composition is per-process.
- **bundles** — pip-installable Python packages discovered via the
  `fantastic.bundles` entry-point group. Each bundle ships a
  `tools.py` with verb handlers + optional `_boot` / `on_delete`
  lifecycle hooks. Idempotent first-boot lets a bundle declaratively
  own its child agents (e.g., a host bundle's `_boot` spawns a
  backend child the first time it boots; on subsequent boots the
  child is already present from disk).
- **web** — HTTP+WS transport bundle (uvicorn). Serves each agent's
  UI at `/{agent_id}/`, proxies WS frames to/from `kernel.send`, and
  serves a tree-shape index at `/` (with ↗ visit links for HTML-
  serving agents and ⊙ reflect popups). The browser frontend brings
  its own typed WS bridge — see the TypeScript frontend kernel below.
- **exec_bridge** — the PROCESS edge of the io family, a general-purpose
  COMMAND runner. Gated like any io_bridge leg and **SEALED BY DEFAULT**
  (`ingress_rule=allow_all_ingress` to open), it spawns ONLY a command line
  matching one of the record's `allowed` regexes — one spawn surface
  (`proc`), no per-call override, no default, no PATH fallback, so a line
  that matches nothing is refused, naming the patterns. A durable program is
  a FILE (written over a stream) plus a command that runs it: `start
  {command, args}` / `status` / `stop` / `interrupt` / `clear` / `boot`,
  the job table, the connector relay and the mail contract are all
  language-blind. A program speaks the kernel over the inherited
  `FANTASTIC_CTRL_FD` (`[4-byte BE len | JSON]`); to get a `kernel` global
  it includes the reference wire client (`exec_bridge/_connector.py`) in
  its own source — the bridge prepends nothing. An allowlisted command is a
  NAMED command, not a sandbox — the operator chose which commands may run,
  nothing
  more.
  Reached like any other unit by id
  (`send(<id>, {...})`), so compute, inference, and memory share one
  calling convention.
- **terminals are exec_bridge PTY jobs** — `start {command, args, pty:{cols,rows}}`
  runs an allowlisted command on a pseudo-terminal (the same gate, the same one
  spawner). The job streams `output` and is driven with `write` / `resize` / `ack`
  (VSCode's flow control: the reader pauses past 100K unacked chars) / `output` (the
  ring, for re-attach) / `paste_image`. The browser xterm view lives in the canvas app
  (the CLI's), not here. Nothing respawns a terminal: a client that attaches `start`s.
- **ws_bridge / relay_connector** — cross-host. Sub-kernel reach is
  the ssh connector + `exec_bridge`; a bridge leg carries the verbs
  so a canvas can reach a remote kernel.
  `ws_bridge` opens WS to a peer kernel's `web_ws`
  and ships raw call frames (asymmetric — no peer bridge needed);
  local agents reach remote agents through it without merging the two
  address spaces, and `watch_remote` streams a remote agent's emits
  back onto the bridge's inbox. Weak proxy: local→local comms stay
  direct.
- **TS frontend kernel** — the browser runs a TypeScript kernel LIBRARY
  (`src/lib/ts`): the same protocol, a pure peer that federates to the host
  over the SAME WS bridge wire (`web_ws`) with its own typed bridge. It is base
  only — no page, no views: an APP boots it (`boot({origin, loader, register})`),
  registers its own `*.ts` bundles and renders what it wants. The canvas
  compositor and its terminal/chat/GL views are such an app, the CLI's
  (`src/tui/web/canvas`). Python knows nothing of either — an app's page is
  served weakly through a `file_bridge` agent over the web host's
  `/<file_id>/file/<path>` proxy, and its records persist back to host disk
  through a `web_loader` (a `kernel_state` with a `root`) via the frontend's
  `proxy_loader`. Recipe: [`src/lib/ts/SERVE.md`](../ts/SERVE.md).

Message flow:

```
                       SERVER                              BROWSER
  agent ──┐                                              ┌── view-agent
           ├── kernel.send / emit / watch ──── WS ──── ┤   (TS frontend
  agent ──┘   (text + binary frames)                    ├──  kernel, a
                                                          │   pure peer)
                                                          └── view-agent
```

## Install

```bash
uv sync                 # builds & installs all workspace bundles editable
uv sync --dev           # + pytest, xdist, etc. (for tests)
```

New bundles dropped under `bundled_agents/` or `installed_agents/`
with a `pyproject.toml` are auto-picked up on the next `uv sync`.

## Run

```bash
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default kernel)
fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=8888    # persist web (one-shot; then `fantastic` daemonizes)
# Equivalent direct invocation: `python main.py [args]`
# main.py bootstraps: Kernel() -> kernel_state.read_tree('.fantastic') -> kernel.load(records).
# Web composition is explicit — no --port flag. The kernel blocks only
# when something keeps it alive (web agent on disk OR REPL stdin loop).
```

REPL example:

```
fantastic> add web port=8888                        # uvicorn boots
fantastic> add file_bridge
# the host renders nothing at `/` — attach a client over web_ws / web_rest
# a browser app (the CLI's canvas) boots the ts kernel and renders its members (see src/lib/ts/SERVE.md)
```

## Drive from outside

`web` is a uvicorn host that serves HTML rendering only. Verb-
invocation surfaces are sub-agents of `web`:

- **`web_ws`** — WebSocket channel at `/<host_id>/ws`. Full duplex:
  `call`, `emit`, `watch`, `state_subscribe`.
- **`web_rest`** — REST diagnostic channel at `POST /<rest_id>/<target_id>`.
  Request/reply only; curl-friendly.

These call legs are **io_bridge derivations: SEALED by default** — a freshly-created
`web_ws`/`web_rest` denies every inbound frame (`{reason:"unauthorized"}`, REST `403`)
until you open it. Compose them per project, opening each consciously:
```bash
fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=8888
# open the WS leg wide for local dev (or `ingress_rule=password_ingress` for a group token):
fantastic kernel create_agent parent_id=kernel_state handler_module=web_ws.tools parent_id=<web_id> ingress_rule=allow_all_ingress
fantastic kernel create_agent parent_id=kernel_state handler_module=web_rest.tools parent_id=<web_id> ingress_rule=allow_all_ingress
```

After `fantastic`:
```bash
# The host renders nothing of its own (`/` is a 404). `/<id>/file/<path>` proxies to
# a gated file_bridge's read_stream (sealed, or a path outside its `allow` list → 404).
curl http://localhost:8888/<id>/file/<path>            # octet proxy → the agent's read_stream

# WS (when web_ws is mounted)
# open ws://host/<id>/ws and send
#   {"type":"call","target":"<id>","payload":{...},"id":"<corr>"}
# mark secret values on the call: add "secret":["payload.<field>"|"result[.<field>]"]
# — or an array of steps for a key holding a dot: "secret":[["payload","a.b"]]
# (REST: the X-Fantastic-Secret header, a JSON array) — every record shows <secret>

# REST (when web_rest is mounted; <rest_id> is the agent's id)
curl -X POST -H 'content-type: application/json' \
  -d '{"type":"reflect"}' http://localhost:8888/<rest_id>/<target_id>
```

The protocol IS the API — no client library. Send
`{"type":"call","target":"kernel","payload":{"type":"reflect","readme":true,"bundles":"all"}}`
to either surface to discover the substrate (agent `tree`, the
installable-bundle catalog, and the root readme with the
transport/wire docs). Any LLM CLI dropped in cold can
bootstrap from one WS or HTTP round-trip.

**Weak binding for bridges.** `ws_bridge` reaches a remote
kernel's `web_ws` by URL + path only — `ws://host/<peer_id>/ws`,
where `<peer_id>` is just the WS path segment (typically `kernel_state`). No
shared Python types cross the wire. (WS-only since the REST bridge
transport was dropped; the `web_rest` diagnostic surface is
unrelated and still ships.)

## Plugin system

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. The kernel
discovers bundles uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins.

Add a bundle by dropping its package under `bundled_agents/` (or
`installed_agents/`) and running `uv sync` — its entry point registers on
the next start, and you can `create_agent parent_id=<owner> handler_module=<bundle>.tools`
from any agent (creates as a child of that agent). A `handler_module` resolves
only from that catalog (plus `register_unlisted` for a test/probe module); a name
outside it is never imported, even when it is on `sys.path`.

## Tests

Two complementary layers:

- **Unit/integration via `pytest`** — fast, parallel, in-process.
  447+ tests including substrate cascade + persistence + reboot.
  ```bash
  uv run --active pytest -n auto         # ~4s parallel
  ```
- **Self-tests** — hand-written, scope-tagged markdown specs. AI
  agents (Claude Code, etc.) read them, ask required pre-flight
  questions, drive the system at the user-facing surface (CLI, HTTP,
  WS, PTY, browser), and fill summary tables. Each component owns
  one. Index + LLM protocol + scope taxonomy at **`selftest.md`**
  (root). Tell the AI things like *"perform non-web self tests"* or
  *"I'm in a canvas, do all webapp tests"* — the AI selects the
  right subset based on scope tags.

## Pre-push checks

CI gates run on PR; mirror them locally before pushing:

```bash
uvx ruff check kernel/ main.py bundled_agents/ tests/
uvx ruff format --check kernel/ main.py bundled_agents/ tests/
uv run pytest -n auto
```

## Layout

```
.                                            # project root
├── main.py                                  # bootstrap: Kernel() -> kernel_state.read_tree('.fantastic') -> kernel.load(records)
├── kernel/
│   ├── __init__.py                          # public API re-exports
│   ├── _agent.py                            # Agent (recursive) + ephemeral flag + on_delete hook
│   ├── _kernel.py                           # Kernel ctx + tree-mgmt API (create/delete/update/list/send)
│   ├── modes/                               # dispatch_argv + one-shots (reflect/call) + default (boot + REPL)
│   ├── _bundles.py                          # entry-point discovery (`fantastic.bundles`) + catalog-only `import_handler`
│   ├── _lock.py                             # serve lock (.fantastic/lock.yaml)
│   └── _env.py                              # .env autoloader
├── pyproject.toml                            # workspace + bundle deps
├── selftest.md                               # selftest INDEX + LLM protocol
├── conftest.py                               # pytest fixtures
├── tests/                                    # substrate-level tests
└── bundled_agents/
    ├── loader/kernel_state/                  # the ROOT agent + persistence/hydration root (owns .fantastic/)
    ├── loader/static_loader/                 # declarative roster loader (a fantastic.yaml manifest)
    ├── cli/                                  # stdout renderer (ephemeral — composed when isatty)
    ├── web/                                  # HTTP+WS transport (uvicorn) + favicon
    ├── io/file_bridge/                       # the DISK edge — sealed by default, deny-all `allow` list, text verbs (read/write/edit/append/undo) + streams; `fs.py` is the one opener
    ├── scheduler/                            # recurring tasks
    ├── io/exec_bridge/                       # the PROCESS edge — sealed-by-default command runner, regex command ALLOWLIST (`allowed`), one spawn surface (proc.py), control wire; a program is a FILE + a command
    ├── memory/ram_store/                     # the MEMORY STORE in RAM — every message (AI conversations, the kernel's execution rows), grants, inject/dump (ram_store.tools)
    ├── ai/ai_core/                           # shared AI agent state (queue/FIFO lock/menu cache, prompt assembly, the ONE-pass _run, `<call>` parsing, verb bodies); every AI backend binds to it
    ├── ai/ollama/ollama_backend              # local LLM (ollama) — thin binding over ai_core
    ├── ai/responses/responses_backend        # OpenAI Responses-API LLM (responses_backend.tools) — thin binding over ai_core
    ├── ai/vllm/vllm_backend                  # vLLM server, OpenAI chat wire, direct (vllm_backend.tools) — thin binding over ai_core
    ├── io/io_bridge/                         # the IO base — a shared LIBRARY, not an agent (channel model, rule registries, read loop/verbs/lifecycle, binary-frame codec, MemoryTransport); bridges + web legs derive from it; IO legs sealed by default
    ├── io/web_ws/, io/web_rest/              # the web call legs (WS / REST) — io_bridge inbound derivations, sealed by default
    ├── io/ws_bridge/                     # cross-kernel WS bridge (asymmetric) — ws transport
    ├── io/relay_connector/               # cross-kernel bridge through a relay-KERNEL router — dial ws://host/<guid>, X-Fantastic-Auth, reach a partner_guid; tunnels io_bridge frames; auto-reconnect + keepalive + directory (list_peers/watch) + kernelgroup typing (role/owner_guid/exposes via set_identity)
```

The browser view layer (canvas compositor + terminal/chat views) is an
app on the TypeScript kernel library (`src/lib/ts`) — the CLI's canvas —
served weakly through generic agents; see [`src/lib/ts/SERVE.md`](../ts/SERVE.md).

## Universal verb

Every agent answers `{type:"reflect"}`, returning the addressed agent
uniformly: `{id, sentence, verbs:{name:doc}, …flat state}`. Root is NOT
special. Compose the reply with flags:

- `tree=all|ids|none` (default `all`) — nested distilled subtree;
  `ids` = flat descendant-id index; `none` = just this agent.
- `bundles=all|ids|none` (default `none`) — the installable-bundle
  catalog (what you can `create_agent` from); `ids` = names only.
- `readme=true` — attach the agent's
  readme.md. On the kernel/root this is the root readme: every
  transport, the wire/binary-protocol details, the reserved `kernel` address, the
  two-kernel model. Transport docs live there now, NOT in the reflect
  JSON — reach them with `reflect readme=true`.

The defaults give a caller enough to navigate and choose; deep details
are fetched on demand.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
