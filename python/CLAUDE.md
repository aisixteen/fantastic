# Fantastic Kernel — Claude Code working notes

A medium that unifies humans and AIs into a single workspace. Recursive
`Agent` class + a `Kernel` shared-context object, one primitive
(`send(target_id, payload)`), plugin-discovered bundles. Every agent
answers `{type:"reflect"}` — the universal discovery verb.

> **NO FALLBACKS — this is a kernel, not a SaaS. Build it fallback-less, close to
> the Linux kernel.** Every operation has exactly ONE path. If a precondition (a
> wired provider, a gate, a dependency) is absent, the operation is a **no-op or
> fails** — it NEVER silently routes around the gap via a second mechanism. No
> `try X; else do Y`, no `if not provider: write_directly`, no "prefer X else Y",
> no defensive default that papers over a missing dependency. Example: with no
> persistence provider wired, `kernel_state` does NOT auto-persist — the tree stays
> in RAM (lost on restart) until you wire one. That is correct. Fallbacks add
> complexity, hide bugs, and create two truths; a kernel refuses them. Bootstrap
> cold primitives (the one-time root seed, `read_tree` at boot) are the single cold
> path, not fallbacks.

> **This is the canonical reference implementation** of the Fantastic
> protocol. When other runtimes (`swift/`, the Apple app's embedded
> kernel) disagree with this kernel on wire shape, on-disk format,
> verb payloads, or reflect output, the other runtime is wrong. The
> protocol surface lives in this file (sections "Architecture",
> "Universal patterns", "Storage policy") — no separate spec doc.
> Cross-runtime drift is caught mechanically by
> `swift/Tests/FantasticParityTests`, which spawns this kernel and
> byte-diffs replies.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│         SUBSTRATE  (kernel/_agent.py + kernel/_kernel.py)                │
│   Agent  — recursive node; .send / .emit / .create / .delete             │
│   Kernel — tree-wide ctx (flat agents index, state stream, save/load)    │
│   System verbs (create/delete/update/list_agents) baked into Agent.      │
│   Persistence DECOUPLED — `kernel_state` (root) is a weak-bound STREAM      │
│   CONSUMER: it auto-persists records THROUGH a `file_bridge` provider's     │
│   write_stream (ephemeral `kernel_store`); cold read stays direct.          │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (agent.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌──────────┐    ┌────────────┐              ┌────────────────┐
   │ kernel_state│    │ web        │              │ python_runtime │
   │  (ROOT)  │    │ (uvicorn)  │              │ terminal · ai  │
   │ cli·file │    │ HTTP + WS  │              │ runners · ...  │
   └──────────┘    └─────┬──────┘              └────────────────┘
                         │
                         ▼ HTTP + WS frames (text + binary)
   ┌─────────────────────────────────────────────────────────────────────┐
   │                    BROWSER — FRONTEND KERNEL                         │
   │  A pure peer (top-level `ts/`, `*.ts` bundles) that federates to the │
   │  host over the SAME WS wire (web_ws). The VIEW + content agents live │
   │  HERE (canvas compositor + its `*.ts` view/content bundles). Their   │
   │  records persist back to host disk under                             │
   │  `.fantastic/web/<session>/` via the frontend's `proxy_loader`.     │
   └─────────────────────────────────────────────────────────────────────┘
```

**No client library. The protocol IS the API.** A code agent (Claude,
LLM CLI) bootstraps from `.fantastic/readme.md` on disk, or from a single
WS `kernel.reflect` round-trip (open `ws://host/<any-agent>/ws`, send a
`call` frame with `target:"kernel", payload:{type:"reflect", readme:true}`).

## Composition principle — NO architectural automation

The system is built by **conscious operator decisions** (the LLM/human composing
it). NO bundle auto-spawns children; NO agent acts autonomously. The **only
autoagent is the loader** (persistence/hydration): the root `kernel_state` on disk +
its peers (a host `web/kernel_state` for the frontend, the JS `proxy_loader`)
mechanically save what the operator built and restore it. Everything else —
composition, membership, wiring — is an explicit `create_agent` / `update_agent` /
`delete_agent`. This is *why* binding is weak (agents reference peers by id, never
couple), why setup is documented procedure (create a `web` agent, then create its
`web_ws` / `web_rest` / `kernel_state` children), and why the host knows nothing of
the frontend. Declared-config metas the substrate wires (not behavior): `alias`
(reach an agent by a stable name), `root` (a loader serves a sub-namespace),
`watch` (a loader only answers verbs), `children_dir` (the on-disk container dir
name — default `agents`; set `host_agents` / `web_agents` for a self-describing
layout). The JS kernel is the SAME kernel — same fields, same flexibility (its
loader lays out the disk). When you reach for "X should automatically do Y,"
stop: the operator does Y, consciously.

## The substrate as a workflow medium (meta)

What this system *is*, beyond "a tree of agents": a medium where **compute,
inference, and memory are interchangeable units, addressed by id, wired from
anywhere — across kernels.** Read it on the two-kernel example (host Python +
browser JS, peers over one WS):

- **Three unit kinds, one calling convention.** `python_runtime` (a background
  compute job), an AI backend (one inference turn), and `yaml_state` (durable
  memory) are all reached the same way: `send(<id>, {type, ...})`. A workflow step
  written as code and the same step written as an LLM call are **substitutable** —
  swap a `python_runtime.start` for an `ai.send` and the wiring is unchanged. This
  is the point: anywhere you'd hand-write a classifier / decision / transform, you
  can drop in an LLM call, and vice-versa.

- **Routines orchestrate the whole substrate, from either kernel.** Out-of-process
  code gets a kernel-mirroring **connector** that talks ONLY to its spawner, which
  holds the live kernel and relays (child → spawner → kernel; never a direct host
  dial — the no-bypass rule). A host `python_runtime` job's spawned code gets a
  `kernel` object (send/emit/reflect/watch/on_message over a socketpair); a browser
  view-agent's JS gets `fantastic` (the same surface over postMessage). Different
  wire, one protocol — exactly like `web_ws` and the pipe are two transports of the
  one envelope. So from EITHER side a routine reads memory anywhere, calls an AI,
  spawns a job, pushes to a panel — by id, regardless of which kernel owns the
  target. In-process `.tools` agents already hold the real `kernel`; the connector
  exists only to give *out-of-process* code the same reach.

- **Routing is emergent, not plumbed.** An AI worker is a streaming unit: it
  receives a call, thinks (calling agents as tools), and streams `token`/`done` on
  its own id. How its result reaches its addressee is the *model's* decision — the
  per-call prompt names who listens (possibly many), the system prompt carries the
  `send` signature, and a capable model routes its own output. No `reply_to`
  primitive; 1:N falls out for free. A recursion guard (`_call_stack`, refused
  before the lock) keeps AI→AI chains from deadlocking or running away.

- **Weak binding everywhere.** Backends and views are weak peers by id — an AI
  worker (or a PTY) runs headless with no view; a view attaches/detaches without
  touching the backend. Bind by id + duck-typed verbs, never by concrete type.

- **Capability emerges from self-description.** None of the above is special-cased:
  it falls out of `send`/`reflect` + each agent's readme. An LLM given only the
  readmes can weave the wiring itself — that's the test bar, not bespoke glue.

- **Validated — emergent memory with judgment.** Told ONLY that a `yaml_state`
  memory agent exists, an AI saves salient facts, withholds trivia (no excess
  writes), recalls them on a FRESH history-less turn, updates them, and prunes
  precisely — managing durable memory through `send` alone; the which-verb/when is
  derived, not coded. Proof: `integration_tests/memory/test_ai_memory_judgment.py`
  (asserts on the store, not prose). NOTE: today only the Swift FM backend
  auto-injects memory each turn; the Python backends reach it on demand by id.

## Run

```bash
uv sync                                              # install workspace + bundles editable
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default: kernel)
fantastic kernel_state create_agent handler_module=web.tools port=8888    # persist web record (first time)
```

`main.py` bootstraps the substrate: `Kernel()` → `kernel_state.read_tree`
reads `.fantastic` → `kernel.load(records)`. The ROOT agent IS an
`kernel_state` (`id="kernel_state"`) — the persistence/hydration root that owns
`.fantastic/`; a fresh dir seeds it. Then argv goes to `dispatch_argv`
(`kernel/modes/`):
  - one-shot: `<id> <verb> [k=v]` / `reflect [<id>]`
  - long-running default: boots every persisted agent. If a `web`
    agent is among them, acquires lock + blocks (uvicorn lives via
    its asyncio task). If stdin is a tty, runs the REPL stdin loop.
    Composing neither → exit silently.

Web composition is **explicit** — no `--port` flag. To make `fantastic`
serve HTTP, persist a web agent first (one-shot create_agent or
REPL `add web port=N`). Next invocation boots it as a daemon.

**The web call legs SEAL by default — open them consciously.** A `web` host
serves only render/static routes; its call surfaces (`web_ws`, `web_rest`) are
`io_bridge` inbound derivations and a freshly-created leg with **no rule denies**
(a browser can't connect, REST returns `403`) until the operator opens it:

```bash
# local dev — open the WS leg wide:
fantastic <web> create_agent handler_module=web_ws.tools ingress_rule=allow_all
# shared group — require the group token (from $FANTASTIC_GROUP_TOKEN):
fantastic <web> create_agent handler_module=web_rest.tools ingress_rule=password
```

Without the `ingress_rule`, the leg mounts but every inbound frame/request is
denied (`{reason:"unauthorized"}`) — opening is a deliberate act
(G2: sealed by default). Same for a bridge leg (`ws_bridge`/`relay_connector`): set
`ingress_rule` to admit inbound calls.

**Octet serving** — `web` bakes in `GET /<agent_id>/file/<path>` (see `app.py`):
any agent answering `read`/`read_stream` becomes an HTTP file server. It prefers
the SOURCE stream verb (`read_stream`) so a LARGE file pipes out chunk-by-chunk;
the **allowance is the served agent's own gate** (a sealed `file_bridge` denies →
`404`), path clamped to its root. The PUMP (`file_bridge.pump`) is the server-side
SOURCE→SINK copy. (Minted-alias URL tier — an opaque token for one file — TBD.)

The bootstrap wires the stdout renderer (Cli) when stdin is a tty —
ephemeral, never persisted.

**First contact (tty / PTY).** On a tty — a human, or an LLM dropped into a
`terminal_backend` PTY — the daemon greets whoever lands there (the tty twin of
the container's HTTP head page). The renderer (`cli`) is a **DUMB SINK** — it
prints what it is told and NEVER inspects the tree. The flow:
  - **`intro_booting`** (kernel → cli, before boot): identity
    (`runtime · env · version · root · pid`) + the **pull/push control-plane
    map** — one envelope `send(<id>,{type})`, PULL (REST / REPL), PUSH (WS
    watch/emit/state_subscribe), REACH compute/infer/memory/shell by id — and a
    pointer to the full map (`reflect readme=true`). Port-independent.
  - **boot-event convention:** each agent announces its OWN endpoints during
    boot. `web`, once it binds, sends cli a `say` with its listening URL (Rust:
    publishes a `say` state event the cli subscriber renders) — the producer
    owns the info; the sink never reaches in for it.
  - **`booted`** (kernel → cli, after the boot loop): the "all booted" close.
`longrun` fires `intro_booting`/`booted` only on a tty; non-tty (the container
daemon) keeps the plain `[kernel] up`. **Best-effort:** no renderer or a race is
fine — everything is in the intro map + `reflect readme=true`. The verbs/text
live in the `cli` bundle; the kernel stays decoupled (it sends verbs, never
imports the bundle). Rust mirrors it: the `fantastic-cli` binary prints
`fantastic_cli_bundle::intro_booting`/`booted`.

REPL example:

```
fantastic> add file_bridge
fantastic> add python_runtime
fantastic> @kernel_state list_agents
```

The view layer (canvas compositor, terminal/chat views, gl/html content
agents) is the TS FRONTEND kernel in the repo's top-level `ts/` package —
a federated peer over the WS bridge, NOT host agents (`*.ts` bundles, see
`ts/SERVE.md`). The host knows nothing of the `ts/` bundles; frontend
records persist back to host disk under `.fantastic/web/<session>/` via
the frontend's `proxy_loader`. See "Two kernels" in the root readme.

## Bundles (current set)

| bundle | role |
|---|---|
| `kernel_state` | the ROOT agent (`id="kernel_state"`) AND the persistence/hydration root: owns `.fantastic/`; answers `load_tree` / `persist_record` / `forget_record`; auto-persists the live tree by subscribing to the kernel state stream (debounced flush). A `root` meta lets a non-root instance serve a sub-namespace (the host-side of the frontend's `web/<session>/`). System verbs are native to Agent; the bootstrap wires Cli when `stdin.isatty()`. |
| `cli` | singleton child of root; renders token/done/say/error events to stdout |
| `web` | uvicorn HTTP host. Serves rendering only — `/` (root index from `templates/index.html`), `/<id>/` (agent's `render_html`), `/<id>/file/<path>` (read-verb file proxy), favicon. Call surfaces (WS, REST) live in sibling sub-agents and mount via the duck-typed `get_routes` verb. The TS frontend kernel brings its own typed WS bridge (`ts/`) — no transport script is injected here. |
| `web_ws` | WebSocket verb-invocation surface — the **ws INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `/<host_id>/ws` on its parent web's FastAPI app. Gates every inbound frame at the shared `gate_inbound` choke point with the leg's own `ingress_rule` (credential on the frame envelope `auth_token`); `egress_rule`/`auth` per leg as on a bridge. **SEALED BY DEFAULT** — a freshly-created `web_ws` with no rule denies (a browser can't connect until opened); open it consciously (see "Run"). Opt-in: `create_agent handler_module=web_ws.tools parent_id=<web> ingress_rule=allow_all`. |
| `web_rest` | REST diagnostic surface — the **http INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `POST /<self_id>/<target_id>` body=payload → kernel.send → JSON reply (+ `GET /_reflect` shortcuts). Gates each request via the leg's `ingress_rule` at `gate_inbound`; the credential rides the **`X-Fantastic-Auth` header** (http carrier, not the envelope). **SEALED BY DEFAULT** — absent rule ⇒ deny (`403 {reason:"unauthorized"}`); open with `ingress_rule=allow_all`/`password`. Multiple instances coexist with different ids. Opt-in. |
| `io/file_bridge` | filesystem-as-agent (`read`, `write`, `list`, `delete`, `rename`, `mkdir`) — the **fs edge of the io family**: crossing kernel↔disk is gated like any io_bridge leg (same `ingress_rule`/`egress_rule`/`auth` fields + registries), **SEALED BY DEFAULT** (absent rule ⇒ every verb except `reflect` denies `{reason:"unauthorized"}`; open with `ingress_rule=allow_all`), and bound by the **running-dir law**: root + every path are clamped inside the dir the kernel runs in (`../`, `~`, absolute escapes, outward symlinks refuse) |
| `scheduler` | recurring tasks; persistence routed through `file_bridge_id` |
| `python_runtime` | async Python JOB spawner — `start` runs `python -u -c <code>` in the background (many in parallel), returns a `job_id` at once + streams `progress`/`job_done` events; `status`/`stop`/`interrupt`/`clear` by job_id over a RAM job table (`on_delete` kills the owner's jobs). No blocking run-and-wait. |
| `yaml_state` | durable YAML key-value memory agent; mount anywhere (`mode=mem|data`); persists `state.yaml` THROUGH a gated `file_bridge` agent referenced by `file_bridge_id` (deny-all by default; `set`/`delete`/`replace` failfast until wired+opened) — owns no disk surface of its own, like `scheduler`/`ai_core` |
| `terminal_backend` | PTY shell backend (at `bundled_agents/terminal/`). VSCode-ported terminal robustness: streaming flow control (reader pauses past 100K unacked chars, resumes on the `ack` verb), incremental UTF-8 decode (no split-char `<?>` litter across `os.read` chunks), serialized full-buffer writes (bracketed-paste-safe), image-paste bridge (browser-clipboard image → `paste_image` → file saved + path typed into the PTY for a CLI like `claude`). The xterm view (`terminal_view`) lives in the TS frontend kernel (`ts/`). |
| `ai/ollama/ollama_backend` | local LLM agent (ollama); per-client chat threads, FIFO lock, menu cache |
| `ai/nvidia/nvidia_nim_backend` | NVIDIA NIM LLM agent (OpenAI-compatible); api_key sidecar via `file_bridge_id`; rate-limit retry; same surface as ollama_backend |
| `ai/anthropic/anthropic_backend` (`anthropic_backend.tools`) | Claude LLM agent — Anthropic Messages API (default model `claude-opus-4-8`); key from `ANTHROPIC_KEY`/`ANTHROPIC_API_KEY` (env / `.env`); per-client chat threads, FIFO lock, native tool-calls, menu cache; same surface as ollama_backend |
| `io/io_bridge` | the **IO base — pure shared library, NOT a registered bundle/agent** (no entry point, never instantiated; the merge of the former `io_core` + `bridge_core` libs). Holds the channel model (`direction · modality · transport · rule · extractor`), the `ingress_rules`/`egress_rules` registries, the transport-agnostic bridge engine (read loop, the 6 verbs, lifecycle, `make_verbs` factory), the unified `gate_inbound`/`stamp_egress` choke point, and the in-process `MemoryTransport`. Every derivation (`ws_bridge`/`relay_connector`/`web_ws`/`web_rest`/`file_bridge`) **imports** from it and reuses `rule.authorize`. IO legs are **SEALED BY DEFAULT** (`resolve_ingress({})` ⇒ `DenyInbound`); the open interior is unaffected. A denied client is taught by the denial's `hint` (the inline recipe — `update_agent <id> ingress_rule=allow_all`) and `reflect readme=true` on the **sealed agent itself** (each derivation ships its own short readme). |
| `io/ws_bridge` | cross-kernel comms — a thin **WS-only, asymmetric** transport **derivation of `io_bridge`** (was `kernel_bridge`). A bridge agent opens a WS to the remote's `web_ws` surface and ships raw `{type:"call", target, payload}` frames; the remote dispatches `kernel.send` exactly like a browser frame and replies over the same socket — **no peer bridge needed**. Transports: memory (test backbone) / ws / ssh+ws. Streaming via `watch_remote` (`{type:"watch", src}` out, `{type:"event"}` back, re-emitted on the bridge's own inbox). All transports are **weak binding** — addressed by URL + path only; no shared Python types with the remote kernel. Weak proxy: local→local stays direct. **Authorization**: two symmetric typed per-leg rules (enforced on the receiver) — `ingress_rule` (inbound FILTER, gated at the shared `gate_inbound` choke point) + `egress_rule` (outbound DECORATOR, stamps a credential on the frame envelope); `auth` is a shorthand for both. **SEALED BY DEFAULT** — absent rule ⇒ `deny_inbound` (reply `{reason:"unauthorized"}`); open consciously with `ingress_rule=allow_all`/`password` (kernel-GROUP shared secret from an env var). Resolved by name from the `io_bridge.ingress_rules` / `egress_rules` registries; shared with relay_connector. |
| `io/relay_connector` | cross-kernel comms through a **relay-KERNEL router** (`../fantastic_relay`). Same verbs/frames as `ws_bridge` (both ride the shared `io/io_bridge` engine). A connector dials the relay at `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, group password in the `X-Fantastic-Auth` header, checked once at the WS upgrade) and reaches a partner kernel by its `partner_guid`; the relay routes by `target` and delivers peer→peer ONE-WAY as `{type:"event", source, payload}`. The connector **TUNNELS** the bridge frames inside relay `{type:"send", target:partner, payload:<frame>}` (out) / unwraps partner `event.payload` (in), so forward/reply correlation + symmetric serving work UNCHANGED — both peers run a connector. Raw `read_stream` chunks ride as native binary WS frames (no base64; the relay forwards the frame kind). **No certs / mTLS / token-issuer** — the relay auths the connection via the header password and routes by GUID. Self-healing: auto-connect + auto-reconnect (`reconnect` meta, default 10s; `0`=one-shot), `keepalive` every `heartbeat`s (default 30), `reflect.connected`=live socket. Directory passthrough (the relay's own `relay` agent, `target:"relay"`): `list_peers` → `{peers:[{guid,status,last_seen,since}]}`, `watch_directory`/`unwatch_directory` re-emit `peer_joined`/`peer_left`/`peer_evicted`/`peer_status` on the connector's inbox. All three host runtimes (python/rust/swift) interop any-to-any — proven by `integration_tests/relay_e2e` against a live `relayd`. Our own `guid` (the WS path) **auto-mints on first boot if absent and persists** into the record (explicit `guid` wins, a minted one is never regenerated — stable across reboots); `partner_guid` stays explicit (discovered via the partner's `reflect`/`list_peers`). **Directory typing (kernelgroup):** a connector advertises an opaque **attrs blob** to the relay on connect (re-announced each reconnect) — well-known keys `role` (`manager`\|`kernel`, default kernel), `owner_guid` (managing peer, null=standalone), `exposes` (control-surface list); the relay stores + reflects it (into `list_peers` entries' `attrs` + a `peer_updated` event) and NEVER interprets it. `set_identity` (verb: optional role/owner_guid/exposes, merged) updates live + persists. reach ≠ control — driving another manager's kernel is a plain `forward` to that manager. Record fields: `relay_url` · `guid` (auto-minted if absent) · `partner_guid` · `relay_token` (X-Fantastic-Auth) · `heartbeat` · `reconnect` · `role` · `owner_guid` · `exposes`; `transport="relay"`. Per-leg `ingress_rule`/`egress_rule` (or the `auth` shorthand) gate the **tunneled** calls and are INDEPENDENT of the relay connection auth — directional/hub-spoke topology + kernel-group `password` membership (shared secret from env); IO legs are **SEALED BY DEFAULT** (absent rule ⇒ `deny_inbound`), opened consciously (`ingress_rule=allow_all`/`password`). |
| `local_runner` (`local_runner.tools`) | local sub-`fantastic` lifecycle — `start`/`stop`/`restart`/`status`/`get_webapp` for one project dir; truth read from the project's `.fantastic/lock.json` (PID) + its web agent record (port) |
| `ssh_runner` | remote `fantastic` lifecycle over SSH — start/stop/restart/status + local SSH tunnel for canvas iframing. Pure subprocess ssh; composes with `ws_bridge` for messaging |

**View bundles live in the FRONTEND kernel (`ts/`), not here.** The
canvas compositor and its view/content agents are `*.ts` bundles that
run in the browser and persist back via `proxy_loader` (see "Two
kernels" in the root readme). The host holds only data/compute/transport
bundles.

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. `fantastic`
discovers them uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins (drop in `installed_agents/`).

Add a bundle by dropping its package under `bundled_agents/` (or
`installed_agents/`) and running `uv sync`; its entry point is scanned at
the next process start.

## Universal patterns

- **`reflect`** — every agent answers `{type:"reflect"}` returning the
  ADDRESSED agent uniformly: `{id, sentence, display_name, description?,
  verbs:{name:doc}, emits:{type:shape}, ...flat state}`. Root is NOT
  special (no `primer`). Compose the reply with flags:
  `tree=all|ids|none` (default `all` — nested distilled subtree; `ids` =
  flat descendant-id index), `bundles=all|ids|none` (default `none` —
  the installable-bundle catalog), `readme=true` (attach the agent's
  readme.md). Verb signatures live in
  docstrings; `reflect` derives them. Discovery is one round-trip;
  transport/wire docs live in the root readme (`reflect readme=true`).
  Each distilled **tree node** also carries the wiring/posture meta it
  has — `ingress_rule`/`egress_rule`/`auth` (a leg's lock: `allow_all` =
  open, `password` = gated, `deny_inbound` or **absent on an io leg** =
  sealed-by-default), `root` (a file_bridge's served dir), `file_bridge_id`
  (what a consumer persists THROUGH) — so the whole IO landscape (which
  legs are open vs sealed, what's wired to what) is readable from ONE root
  reflect, no per-agent round-trips.
  The **ROOT** reflect additionally carries `runtime` — a stable lowercase
  enum (`python` | `rust` | `swift` | `ts`) naming the kernel's runtime, so
  a client gates runtime-specific UI from one round-trip. Same field name +
  values across all four runtimes; injected uniformly in
  `_apply_reflect_flags` (root only). Differs by runtime BY DESIGN (like the
  root id), so cross-runtime parity asserts the per-runtime value, not equality.
  The root reflect ALSO carries two deployment-context fields, in key order
  `runtime → env → version`: `env` (read from `$FANTASTIC_ENV`, default
  `"host"`; the container image bakes `"container"`) tells a client WHERE the
  kernel runs — e.g. `env:"container"` means `shutdown_kernel` stops + (under
  `--rm`) removes a whole container, not just a process — and `version` (read
  from `$FANTASTIC_VERSION`, default `null`; the image bakes the release tag)
  names the build. Both are RUN-scoped (read inline at root reflect, never
  persisted to the portable `.fantastic` workdir, which can move host↔container)
  and root-only like `runtime`. The host runtimes read the envs; the browser
  `ts` kernel has no OS env, so it is always `env:"host"`, `version:null`.
  The root reflect ALSO carries `persistence: {provider:<id>|null}` — WHICH
  file_bridge the loader auto-persists records THROUGH (the discovered store),
  or `null` = nothing wired (state in RAM). Contributed by the loader via a
  duck-typed `reflect_root_extra(agent)` hook (no substrate↔bundle coupling),
  so a client sees in one reflect whether persistence is wired — and, with the
  provider's posture inline in the tree, whether the wired leg is actually open.
- **`render_html`** — duck-typed presentation. Any agent returning
  `{html:str}` from `render_html` can be rendered by a view. This is now
  a FRONTEND pattern (a `*.ts` content agent holds the body in its
  record); the host web still exposes a generic `/<id>/` route but ships no host
  implementer. Bodies reach the kernel via the frontend's own typed WS
  bridge (`ts/`) — no transport script injected.
- **`get_webapp`** — duck-typed UI discovery. The TS canvas compositor
  iframes any agent that answers `get_webapp` with `{url,
  default_width, default_height, title}`.
- **`reload_html`** — universal page reload. The TS frontend kernel
  subscribes to it; any agent that emits `{type:"reload_html"}` on its
  own inbox triggers a reload of the connected view. `set_html` and the
  canvas frame ⟳ button both go through it.
- **`file_bridge_id`** — bundles that need persistence (ollama_backend,
  scheduler) carry an `file_bridge_id` on
  their record. Failfast if unset (no implicit fallback).
- **`delete_lock: true`** on a record refuses delete. The root's
  `delete_agent` returns `{error, locked:true, id}` so LLM callers
  can detect it programmatically. Clear via `update_agent`.
- **`on_delete` cascade hook** — substrate calls `await
  agent.on_delete()` depth-first during cascade-delete BEFORE
  detaching the record from `kernel.agents` and the parent's
  `_children`. It tears down PROCESS state only: if the agent's
  `handler_module` exposes `async def on_delete(agent)`, invoke it.
  DISK cleanup is NOT here — the `removed` state event drives a loader
  to rmtree the dir. Bundles port teardown into this function —
  `terminal_backend.on_delete` closes the PTY, `web.on_delete` drains
  uvicorn, `ws_bridge.on_delete` cancels the read loop + tunnel,
  runner bundles call their own `_stop`.
- **`ephemeral` class flag** — `class Cli(Agent): ephemeral = True`
  means a loader skips it entirely (never written to disk; `save()`
  omits it). Composition is per-process; reboots compose afresh based
  on mode. Use for stateless renderers / debuggers / dispatchers.
- **State-event `sender` + `summary`** — every `send`/`emit` state
  event carries `sender` (the dispatching agent's id, set via a
  task-local contextvar around handler dispatch; the host's WS/REST
  surface tags external traffic with the surface agent's own id) and
  `summary` (a JSON-stringified, bytes-stripped, max-160-char view
  of the payload). A frontend telemetry view uses both to draw
  sender→receiver wires + a last-N message log.
- **Iframe URLs — never hardcode a host** — agents that return a URL
  from `get_webapp` use a path-relative form (`/<agent_id>/`) so the
  iframe inherits the canvas's `host:port` automatically. Wrappers
  that embed an **external** HTTP service on a different port (the
  vscode_fantastic bundle wraps `code serve-web`; future Jupyter /
  media-server / remote-app bundles will look similar) must build
  the iframe URL with `window.location.hostname`, not the host
  serve-web bound to. Browsers treat `localhost` and `127.0.0.1` as
  different sites (Safari especially partitions storage / cookies /
  SAB across them), and a cross-site top↔iframe relationship trips
  workbench-style apps. Hostname-matching keeps top-doc + iframe
  same-site without exposing the bind on a public interface, and
  composes naturally with the `ssh_runner` tunnel (the tunnel
  terminates on the user's localhost, so the hostname stays
  consistent).

## Self-bootstrap (for code agents)

The substrate is self-describing through the root readme — which a code
agent gets either by reading `.fantastic/readme.md` off disk (no process,
no socket needed) or with one reflect:

Open `ws://host/<any-agent>/ws` and send
`{"type":"call","target":"kernel","payload":{"type":"reflect","readme":true,"bundles":"all"},"id":"1"}`.
The reply carries:

- `readme` — the root readme: every transport (in_process / in_prompt /
  cli / ws / rest / binary-frame), the reflect surface, the `kernel`
  alias, the two-kernel (host + frontend) model, and the
  `.fantastic/lock.json` daemon rule. This is where transport/wire docs
  live now (they left the reflect JSON).
- `tree` — the live agent tree (default `all`; `tree:"ids"` for a cheap
  id index).
- `bundles` — with `bundles:"all"`, every installable bundle (what you
  can `create_agent` from); `bundles:"ids"` for names only.

Per-agent reflect carries `verbs: {name: doc-line}` so an LLM caller can
compose any `payload` from the docstring without source diving. "If you
find yourself reading kernel/ to discover a transport URL, that's a
regression — it belongs in the root readme."

## Tests

- **Unit** — `pytest -n auto` (`pytest-xdist`). 420+ tests, parallel,
  in-process. Each bundle's tests live in `bundled_agents/<bundle>/tests/`;
  kernel-level tests live in `tests/`. `conftest.py` at root exposes
  `kernel`, `seeded_kernel`, `file_bridge` fixtures (the root is an
  `kernel_state`); `_testkit.py` adds `boot_root` / `persist` for disk tests.
- **Selftests** — scope-tagged markdown specs. AI agents read them,
  drive the system at the user-facing surface (CLI, HTTP, WS, PTY,
  browser), and fill summary tables. Index + protocol at root
  `selftest.md`. Each bundle owns one.

## Pre-push checks

```bash
uvx ruff check kernel/ main.py bundled_agents/ tests/
uvx ruff format --check kernel/ main.py bundled_agents/ tests/
uv run pytest -n auto
```

## Commits & pushes — ASK FIRST

**Do NOT `git commit` or `git push` unless the user explicitly asks
for it.** No "I'll just commit this since the work is done" — wait
for the word.

OK to do without asking: edit files, run `pytest`, run `ruff`,
create a branch when the user has named it, show diffs (`git diff
--cached`, `git status`). Anything that touches refs or origin
needs explicit consent: commit, amend, push, force-push, branch
delete, tag, rebase, reset --hard.

If you think work should be committed, _say so_ and wait. The cost
of asking once is low; the cost of an unwanted commit (lost diff
review, force-push surprise, polluted history) is high.

## Conventions

- All async (`asyncio` throughout). `pytest-asyncio` with
  `asyncio_mode = "auto"`.
- Agent IDs: `{bundle}_{hex6}` (e.g. `ollama_backend_b04b35`).
  Singletons use the bundle name (`kernel_state`, `cli`).
- Every bundle's `tools.py` defines:
  - per-verb `async def _<name>(id, payload, kernel)` with a
    one-line docstring (auto-fed into reflect's `verbs` dict).
  - `VERBS = {"<name>": _<name>, ...}` dispatch table.
  - `async def handler(id, payload, kernel)` 4-line dispatcher.
- Records in `.fantastic/agents/<id>/agent.json` carry only metadata.
  Process-memory state (PTY child, uvicorn server, in-flight tasks)
  is per-process and visible only to the kernel that owns it —
  reflect from the live `serve` to see it, NOT via a fresh
  `fantastic <id> <verb>` (which spawns a separate kernel).

## Storage policy

- Project code lives in version-controlled files in this repo or
  user dirs. **`.fantastic/` is runtime state only** — agent.json
  records, per-agent sidecars (chat_<client>.json,
  schedules.json, history.jsonl), `lock.json`, `readme.md`.
  Wipe-and-rebuild safe.
- Use a `file_bridge` agent (rooted INSIDE the running dir — the clamp;
  created OPEN with `ingress_rule=allow_all`) for HTTP-served content:
  `<img src="/<file_id>/file/imgs/foo.png">` works in any html view.

## Disk surface — ALL IO through the gated bridge

**The model (the coherent end-state): ALL IO goes through a `file_bridge` AGENT, no
exceptions, deny-all by default, opened on demand by the operator/LLM.** Two layers, both
unbypassable:
1. **The clamp** — `file_bridge.fs` (`io/file_bridge/.../fs.py`) is the SINGLE module that
   calls `open()`; the running-dir law lives there, so nothing can read/write outside its
   root by going around it (there's no other disk code to go around to).
2. **The gate** — the `file_bridge` AGENT wraps `fs` with the io-bridge ingress rule:
   **SEALED / deny-all by default** (every verb except `reflect` denies). A bundle that
   needs disk does NOT import `fs`; it `send`s `read`/`write`/`read_stream`/`write_stream`
   to a file_bridge agent referenced by **`file_bridge_id`** on its record. The operator/LLM
   **wires + opens** that provider on demand (`create_agent file_bridge … ingress_rule=…`,
   then set `file_bridge_id`). So the *access control* is real for in-process bundles too —
   not just the clamp. `yaml_state` / `scheduler` / `ai_core` / `nvidia_nim` already route
   this way; they failfast if `file_bridge_id` is unset (no silent RAM).

The ONLY code that imports `fs` directly is **the file_bridge agent itself** (it IS the
gate) and **the cold bootstrap** — the bring-up *before* any agent/gate exists, not an
exception to access control: substrate fixed-path reads (`kernel/_lock.py`, `_env`,
`Agent._read_readme`; the substrate is decoupled from bundles and cannot import one) +
`kernel_state`'s boot-read of `.fantastic` (`read_tree`, the chicken-egg root hydration).
*In flight toward the end-state:* `local_runner`/`terminal` still import `fs`
(`external=True`) because they reach OUTSIDE cwd (sibling projects, /tmp) — they fold into
the agent model once a file_bridge agent can carry an explicit external root. (`web` is
already clean: `/` is the agent index, `/file/` proxies to gated agents, own assets are
importlib package resources — no `fs`.)

`fs`'s two surfaces, picked STATICALLY per call site (never tried in sequence — that would
be a fallback, which this kernel refuses):
- **clamped** (default) — `fs.read_text(root, path)`: `root` clamped to cwd, `path`
  clamped within `root`. This is the kernel's OWN state (its `.fantastic`). The running-dir
  law guarantees the kernel can't escape its own dir.
- **external** (`fs.read_text(root, path, external=True)`) — `root` is an operator/peer
  base that may live ANYWHERE; `path` still clamped within it. For surfaces that operate
  OUTSIDE cwd BY DESIGN: `local_runner` orchestrating a SIBLING project, `terminal`'s
  clipboard-image temp scratch dir.

The ONLY disk code outside `fs`: the substrate's fixed-path bootstrap (`kernel/_lock.py`,
`_env`, `Agent._read_readme` — the substrate is decoupled from bundles, cannot import one)
and `importlib.resources` reads of a bundle's OWN baked-in assets (readme/help/index/
favicon — a package lookup, not an arbitrary path).

## Path conventions

- All paths relative when invoked via `fantastic <id> <verb>` (cwd = project
  dir).
- The file_bridge's path-safety refuses anything escaping its `root`, and
  the running-dir law clamps the `root` itself inside the kernel's cwd.
- `fantastic` writes `.fantastic/lock.json` with `{pid, port}`;
  a second serve in the same dir refuses with a clear error and stale
  locks (dead pid) get overwritten.

## What's NOT here (yet)

These existed in an older codebase iteration; deferred, replaced, or moved:

- `core` — cut. The root IS the `kernel_state` agent now; there's no
  separate userland-orchestrator class.
- `canvas_backend` / `html_agent` / `gl_agent` — the spatial UI + view/
  content agents are no longer HOST bundles; they moved to the FRONTEND
  kernel (`ts/`, `*.ts`). See "Two kernels".
- `telemetry_pane` — discarded entirely (a throwaway test agent; gone
  from every runtime, not moved).
- `openai` AI bundle — not shipped (`ollama` / `anthropic` /
  `nvidia_nim` backends ship). Pattern: mirror `ollama_backend`.
  Recoverable from git history.
- `register_template` / `list_templates` — replaced by per-agent
  reflect (single source of truth).
- `content_alias_file` registry — replaced by the URL convention
  `/<file_id>/file/<path>`.
- agent `memory_long.jsonl` append-only memory — replaceable by the
  `file_bridge` agent + path convention.
