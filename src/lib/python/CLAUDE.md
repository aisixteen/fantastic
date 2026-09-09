# Fantastic Kernel — Claude Code working notes

A medium that unifies humans and AIs into a single workspace. Recursive
`Agent` class + a `Kernel` shared-context object, one primitive — the ARRAY-ONLY
batch send `send([{target, payload, timeout?}, ...]) -> [{pid, status, result}]`
(single call = `(await send([{...}]))[0]`; statuses
`completed|failed|cancelled|timed_out`; concurrent dispatch, input order
preserved; timeout per item, NEVER defaulted by the kernel, enforced through
`cancel_task` so `on_cancel` hooks run; there is deliberately NO single-call
sugar — one shape prevents drift) — plugin-discovered bundles. Python is ahead
of rust/swift/ts on this contract until `tmp/pychanges.md` #29 is ported. Every agent
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
│   Kernel — tree-wide ctx (flat agents index, state stream, save/load,    │
│            TASK TABLE — every in-flight dispatch, a tree)                │
│   KERNEL verbs (create/delete/update/list_agents + ps/cancel) — the      │
│   ROOT answers them; every reflect ADVERTISES them as `kernel_verbs`.    │
│   into Agent.                                                            │
│   CALL GATE — two-sided deny-all `Agent.send` (kernel/_authz.py): an     │
│   AGENT send needs the sender's `can_call` (egress) AND the target's     │
│   `callable_by` (ingress) — reflect included; OPERATOR (sender=None)      │
│   ungated. Authority descends only (no self-raise).                      │
│   Persistence DECOUPLED — `kernel_state` (root) is a weak-bound STREAM      │
│   CONSUMER: it auto-persists records THROUGH a `file_bridge` provider's     │
│   write_stream (ephemeral `kernel_store`); cold read stays direct.          │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (agent.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌──────────┐    ┌────────────┐              ┌────────────────┐
   │ kernel_state│    │ web        │              │  exec_bridge   │
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

- **Three unit kinds, one calling convention.** `exec_bridge` (a background
  compute job), an AI backend (one inference turn), and `text_file` (durable
  memory as plain files) are all reached the same way: one item in a `send([...])` batch. A workflow step
  written as code and the same step written as an LLM call are **substitutable** —
  swap an `exec_bridge.start` for an `ai.send` and the wiring is unchanged. This
  is the point: anywhere you'd hand-write a classifier / decision / transform, you
  can drop in an LLM call, and vice-versa.

- **Routines orchestrate the whole substrate, from either kernel.** Out-of-process
  code gets a kernel-mirroring **connector** that talks ONLY to its spawner, which
  holds the live kernel and relays (child → spawner → kernel; never a direct host
  dial — the no-bypass rule). A host `exec_bridge` job's spawned code gets a
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

- **Validated — emergent memory with judgment.** Told ONLY that a memory agent
  exists, an AI saves salient facts, withholds trivia (no excess writes),
  recalls them on a FRESH history-less turn, updates them, and prunes precisely
  — managing durable memory through `send` alone; the which-verb/when is
  derived, not coded. (Originally proven on the deleted KV store,
  `integration_tests/memory/test_ai_memory_judgment.py`; memory is now plain
  FILES via `text_file`, and the judgment claim re-validates against the file
  face in e2e.) NOTE: today only the Swift FM backend auto-injects memory each
  turn; the Python backends reach it on demand by id.

## Run

```bash
uv sync                                              # install workspace + bundles editable
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default: kernel)
fantastic kernel create_agent parent_id=kernel_state parent_id=kernel_state handler_module=web.tools port=8888   # persist web record
fantastic init static_loader ./fantastic.yaml        # seed a declarative-roster loader + boot it
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
fantastic kernel create_agent parent_id=kernel_state parent_id=<web> handler_module=web_ws.tools ingress_rule=allow_all_ingress
# shared group — require the group token (from $FANTASTIC_GROUP_TOKEN):
fantastic kernel create_agent parent_id=kernel_state parent_id=<web> handler_module=web_rest.tools ingress_rule=password_ingress
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
imports the bundle). Rust no longer carries a stdout-renderer bundle: its
`fantastic_kernel` binary is a pure headless host (boot + serve + one-shot
verbs); the interactive front-end now lives in the `fantastic` CLI product
(`src/cli/`), which owns the banner, mode tabs, and event log.

REPL example:

```
fantastic> add file_bridge
fantastic> add exec_bridge
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
| `kernel_state` | the ROOT agent (`id="kernel_state"`) AND the persistence/hydration root: owns `.fantastic/`; answers `load_tree` / `persist_record` / `forget_record`; auto-persists the live tree by subscribing to the kernel state stream (debounced flush). A `root` meta lets a non-root instance serve a sub-namespace (the host-side of the frontend's `web/<session>/`). Kernel verbs are answered HERE, at the root, and nowhere else; the bootstrap wires Cli when `stdin.isatty()`. |
| `static_loader` | a DECLARATIVE roster loader (`loader/static_loader`). Reads a `fantastic.yaml` manifest (`manifest` field) and at `boot` hydrates the declared agents as OPERATOR (`operator_context` + normal `create_agent`), as its own children — each carrying the baseline grant + the manifest's explicit `can_call`. The kernel never parses YAML; this bundle owns the format (mirror of `kernel_state` owning the `.fantastic` layout). After boot the roster is CLOSED by deny-all (an agent can't `create_agent`), so the manifest IS the roster. Idempotent by id (a reboot re-reads + skips existing). Seed with `fantastic init static_loader ./fantastic.yaml`. |
| `cli` | singleton child of root; renders token/done/say/error events to stdout |
| `web` | uvicorn HTTP host. Serves rendering only — `/` (root index from `templates/index.html`), `/<id>/` (agent's `render_html`), `/<id>/file/<path>` (read-verb file proxy), favicon. Call surfaces (WS, REST) live in sibling sub-agents and mount via the duck-typed `get_routes` verb. The TS frontend kernel brings its own typed WS bridge (`ts/`) — no transport script is injected here. |
| `web_ws` | WebSocket verb-invocation surface — the **ws INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `/<host_id>/ws` on its parent web's FastAPI app. Gates every inbound frame at the shared `gate_inbound` choke point with the leg's own `ingress_rule` (credential on the frame envelope `auth_token`); `egress_rule`/`auth` per leg as on a bridge. **SEALED BY DEFAULT** — a freshly-created `web_ws` with no rule denies (a browser can't connect until opened); open it consciously (see "Run"). Opt-in: `create_agent handler_module=web_ws.tools parent_id=<web> ingress_rule=allow_all_ingress`. |
| `web_rest` | REST diagnostic surface — the **http INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `POST /<self_id>/<target_id>` body=payload → kernel.send → JSON reply (+ `GET /_reflect` shortcuts). Gates each request via the leg's `ingress_rule` at `gate_inbound`; the credential rides the **`X-Fantastic-Auth` header** (http carrier, not the envelope). **SEALED BY DEFAULT** — absent rule ⇒ deny (`403 {reason:"unauthorized"}`); open with `ingress_rule=allow_all_ingress`/`password`. Multiple instances coexist with different ids. Opt-in. |
| `io/file_bridge` | filesystem-as-agent (`read`, `write`, `list`, `delete`, `rename`, `mkdir`) — the **fs edge of the io family**: crossing kernel↔disk is gated like any io_bridge leg (same `ingress_rule`/`egress_rule`/`auth` fields + registries), **SEALED BY DEFAULT** (absent rule ⇒ every verb except `reflect` denies `{reason:"unauthorized"}`; open with `ingress_rule=allow_all_ingress`), and bound by the **running-dir law**: root + every path are clamped inside the dir the kernel runs in (`../`, `~`, absolute escapes, outward symlinks refuse) |
| `io/exec_bridge` | commands-as-agents (`start`, `status`, `stop`, `interrupt`, `clear`, `boot`) — the **process edge of the io family**: crossing kernel↔subprocess is gated like any io_bridge leg (same `ingress_rule`/`egress_rule`/`auth` fields + registries), **SEALED BY DEFAULT** (absent rule ⇒ every verb except `reflect` denies `{reason:"unauthorized"}`; open with `ingress_rule=allow_all_ingress`), and bound by a **regex command ALLOWLIST**: `allowed` on the record is a list of regexes, and `start {command, args}` refuses unless the shell-joined line fullmatches one — enforced through `proc`, the ONE process surface, exactly as `fs` is the one opener. No per-call override, no default, no PATH fallback; an empty `allowed` runs nothing. It is a GENERAL-PURPOSE COMMAND runner, **not** a code runner: there is no `code` field, no `interpreters`, no shim. A durable program is a FILE some process (or a `write_stream`) put on disk; running it is `command` + `args` like anything else, so the job table / output pump / connector relay / fd passing / task-table rows / mail contract are all language-blind. A spawned program speaks the kernel over the inherited `FANTASTIC_CTRL_FD` (`[4-byte BE len | JSON]`); to get a `kernel` global it includes the reference wire client (`exec_bridge/_connector.py`, `CONNECTOR_SRC`) in its own source — the bridge prepends nothing. Any verb the bundle does not own is MAIL for the running program (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel`, `on_cancel` SIGKILLs the real process); `boot` is a no-op (nothing auto-restarts — a command is started by a caller, not the record). The withdrawn fields (`code`/`shim`/`interpreter`/`interpreters`/`argv_template`) are refused at WRITE time (`validate_meta`) and at `start`. NOT a sandbox: an allowlisted command is a NAMED command, with all of that command's reach. |
| `scheduler` | recurring tasks; persistence routed through `file_bridge_id` |
| `text_file` | stateless TEXT-DOCUMENT agent — durable memory as plain files, ANY format (the substrate imposes none): line-numbered `read`, atomic `write` (tmp+rename on the target), SEARCH/REPLACE `patch` (all-or-nothing, fails loud), `append`; ON DEMAND: `version` hash + `if_match` guard, `checkpoint`/`checkpoints`/`restore`/`drop` (pre-images as plain files under `.ckpt/`, ids = the dispatch's task pid). Owns NO disk: every byte is a GATED send to the `file_bridge` named by `on:` or its `target_id` meta — fails loud unwired. |
| `terminal_backend` | PTY shell backend (at `bundled_agents/terminal/`). VSCode-ported terminal robustness: streaming flow control (reader pauses past 100K unacked chars, resumes on the `ack` verb), incremental UTF-8 decode (no split-char `<?>` litter across `os.read` chunks), serialized full-buffer writes (bracketed-paste-safe), image-paste bridge (browser-clipboard image → `paste_image` → file saved + path typed into the PTY for a CLI like `claude`). The xterm view (`terminal_view`) lives in the TS frontend kernel (`ts/`). |
| `ai/ollama/ollama_backend` | local LLM agent (ollama); per-client chat threads, FIFO lock, menu cache; RAW prompt-and-parse tool-calling (see `ai_core`) |
| `ai/nvidia/nvidia_nim_backend` | NVIDIA NIM LLM agent (OpenAI-compatible); api_key sidecar via `file_bridge_id`; rate-limit retry; same surface as ollama_backend |
| `ai/llamacpp/llamacpp_backend` | LOCAL LLM agent via a `llama-server` (llama.cpp) OpenAI surface — default endpoint `http://127.0.0.1:8080/v1`, keyless unless the server was started with `--api-key`. Kept SEPARATE from `nvidia_nim_backend` despite the shared wire so `reflect` never describes a local model as an NVIDIA agent. The ollama backend CANNOT serve this role: llama-server 404s every `/api/*` route and the ollama SDK posts to `/api/chat`. Reads BOTH stream channels — with the server on `--reasoning-format deepseek` the thought arrives as `reasoning_content` and only the conclusion as `content`, so reading `content` alone drops whole turns. `think` on the record rides as per-request `chat_template_kwargs:{enable_thinking}` (gemma-4 gates its thought channel there, default FALSE), so toggling thinking is a record edit, not a server restart. No `num_ctx` — the window is fixed at server start (`-c N`) and this agent does not pretend to own one. |
| `ai/anthropic/anthropic_backend` (`anthropic_backend.tools`) | Claude LLM agent — Anthropic Messages API (default model `claude-opus-4-8`); key from `ANTHROPIC_KEY`/`ANTHROPIC_API_KEY` (env / `.env`); per-client chat threads, FIFO lock, menu cache; same surface as ollama_backend |
| `io/io_bridge` | the **IO base — pure shared library, NOT a registered bundle/agent** (no entry point, never instantiated; the merge of the former `io_core` + `bridge_core` libs). Holds the channel model (`direction · modality · transport · rule · extractor`), the `ingress_rules`/`egress_rules` registries, the transport-agnostic bridge engine (read loop, the 6 verbs, lifecycle, `make_verbs` factory), the unified `gate_inbound`/`stamp_egress` choke point, and the in-process `MemoryTransport`. Every derivation (`ws_bridge`/`relay_connector`/`web_ws`/`web_rest`/`file_bridge`) **imports** from it and reuses `rule.authorize`. IO legs are **SEALED BY DEFAULT** (`resolve_ingress({})` ⇒ `DenyInbound`); the open interior is unaffected. A denied client is taught by the denial's `hint` (the inline recipe — `update_agent <id> ingress_rule=allow_all_ingress`) and `reflect readme=true` on the **sealed agent itself** (each derivation ships its own short readme). |
| `io/ws_bridge` | cross-kernel comms — a thin **WS-only, asymmetric** transport **derivation of `io_bridge`** (was `kernel_bridge`). A bridge agent opens a WS to the remote's `web_ws` surface and ships raw `{type:"call", target, payload}` frames; the remote dispatches `kernel.send` exactly like a browser frame and replies over the same socket — **no peer bridge needed**. Transports: memory (test backbone) / ws / ssh+ws. Streaming via `watch_remote` (`{type:"watch", src}` out, `{type:"event"}` back, re-emitted on the bridge's own inbox). All transports are **weak binding** — addressed by URL + path only; no shared Python types with the remote kernel. Weak proxy: local→local stays direct. **Authorization**: two symmetric typed per-leg rules (enforced on the receiver) — `ingress_rule` (inbound FILTER, gated at the shared `gate_inbound` choke point) + `egress_rule` (outbound DECORATOR, stamps a credential on the frame envelope); `auth` is a shorthand for both. **SEALED BY DEFAULT** — absent rule ⇒ `deny_inbound` (reply `{reason:"unauthorized"}`); open consciously with `ingress_rule=allow_all_ingress`/`password` (kernel-GROUP shared secret from an env var). Resolved by name from the `io_bridge.ingress_rules` / `egress_rules` registries; shared with relay_connector. |
| `io/relay_connector` | cross-kernel comms through a **relay-KERNEL router** (`../fantastic_relay`). Same verbs/frames as `ws_bridge` (both ride the shared `io/io_bridge` engine). A connector dials the relay at `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, group password in the `X-Fantastic-Auth` header, checked once at the WS upgrade) and reaches a partner kernel by its `partner_guid`; the relay routes by `target` and delivers peer→peer ONE-WAY as `{type:"event", source, payload}`. The connector **TUNNELS** the bridge frames inside relay `{type:"send", target:partner, payload:<frame>}` (out) / unwraps partner `event.payload` (in), so forward/reply correlation + symmetric serving work UNCHANGED — both peers run a connector. Raw `read_stream` chunks ride as native binary WS frames (no base64; the relay forwards the frame kind). **No certs / mTLS / token-issuer** — the relay auths the connection via the header password and routes by GUID. Self-healing: auto-connect + auto-reconnect (`reconnect` meta, default 10s; `0`=one-shot), `keepalive` every `heartbeat`s (default 30), `reflect.connected`=live socket. Directory passthrough (the relay's own `relay` agent, `target:"relay"`): `list_peers` → `{peers:[{guid,status,last_seen,since}]}`, `watch_directory`/`unwatch_directory` re-emit `peer_joined`/`peer_left`/`peer_evicted`/`peer_status` on the connector's inbox. All three host runtimes (python/rust/swift) interop any-to-any — proven by `integration_tests/relay_e2e` against a live `relayd`. Our own `guid` (the WS path) **auto-mints on first boot if absent and persists** into the record (explicit `guid` wins, a minted one is never regenerated — stable across reboots); `partner_guid` stays explicit (discovered via the partner's `reflect`/`list_peers`). **Directory typing (kernelgroup):** a connector advertises an opaque **attrs blob** to the relay on connect (re-announced each reconnect) — well-known keys `role` (`manager`\|`kernel`, default kernel), `owner_guid` (managing peer, null=standalone), `exposes` (control-surface list); the relay stores + reflects it (into `list_peers` entries' `attrs` + a `peer_updated` event) and NEVER interprets it. `set_identity` (verb: optional role/owner_guid/exposes, merged) updates live + persists. reach ≠ control — driving another manager's kernel is a plain `forward` to that manager. Record fields: `relay_url` · `guid` (auto-minted if absent) · `partner_guid` · `relay_token` (X-Fantastic-Auth) · `heartbeat` · `reconnect` · `role` · `owner_guid` · `exposes`; `transport="relay"`. Per-leg `ingress_rule`/`egress_rule` (or the `auth` shorthand) gate the **tunneled** calls and are INDEPENDENT of the relay connection auth — directional/hub-spoke topology + kernel-group `password` membership (shared secret from env); IO legs are **SEALED BY DEFAULT** (absent rule ⇒ `deny_inbound`), opened consciously (`ingress_rule=allow_all_ingress`/`password`). |

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
  the installable-bundle catalog), and `readme` — polymorphic: `true`
  attaches the agent's SHORT orientation `readme.md`; `"index"` returns
  the token-annotated topic tree `[{key,desc,tokens,children?}]`;
  `"<key>"` drills one topic (body + its sub-docs' index); an unknown key
  → `{error,keys}`. Verb signatures live in
  docstrings; `reflect` derives them. Discovery is one round-trip;
  transport/wire docs live in the doc tree (`reflect readme=index`, then
  drill by key). The ROOT reflect always carries `readmes` (the top-level
  topic index) + a `docs` one-liner, so the manual's map rides the first
  reflect.
  Each distilled **tree node** also carries the wiring/posture meta it
  has — `ingress_rule`/`egress_rule`/`auth` (a leg's lock; names are
  MANIFOLD-SCOPED: `allow_all_ingress` = open, `password_ingress` = gated,
  `deny_ingress` or **absent on an io leg** = sealed-by-default. `auth` takes a
  bare STEM and each manifold appends its own suffix, so one name can never
  half-resolve across the two registries), `root` (a file_bridge's served dir), `file_bridge_id`
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
  The root reflect ALSO carries `guid` — the kernel's stable INSTANCE identity,
  minted ONCE at boot (the loader's `_boot`, root only; auto-fills an absent field
  so an explicit `guid` wins and a minted one is never regenerated) and persisted
  into the root record `.fantastic/agent.json`. Contributed by the same
  `reflect_root_extra(agent)` hook, alongside `persistence`. Unlike the run-scoped
  `env`/`version` (read inline each reflect), the guid is PERSISTENCE-scoped — it
  round-trips a reboot verbatim, sourced from the persisted meta. A relay peer
  advertises this value as its `owner_guid` (the kernel-INSTANCE guid — distinct
  from a relay LEG's own `guid` on a `relay_connector` record).
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
- **The TASK TABLE (`ps` / `cancel`)** — the kernel models WORK IN
  FLIGHT, Linux-style. The unit is a **task** — ONE in-flight dispatch,
  not an agent — with an int id (a pid: the message id of its call). Every `send` runs its
  dispatch as a registered `asyncio.Task`; a `send` issued INSIDE a handler
  is a **CHILD** of that handler's task (the current id rides a
  `_current_task` contextvar, exactly as the sender rides `_current_sender`),
  so the table is a **tree**. Two KERNEL verbs read and act on it — baked into
  the substrate, in no bundle's `verbs` map, answered by the ROOT. A task id
  is the message id of the call that started it (an int from the kernel's one
  counter; the agent is a field on the row), which is why neither verb
  needs a target. An agent granted `cancel` on the kernel can kill ANY task:
  before 2026-08-24 every agent answered these, so a manifest could grant
  "A may watch and kill B's work" as one edge — that granularity is gone,
  knowingly, in exchange for one unambiguous address:
  - `ps` → `{tasks:[{id, agent, parent, verb, started_at}], next}` — the live table.
    PAGED like `list_agents` and `reflect tree=ids`: pass `limit` (+ `after`,
    the opaque cursor a previous page returned) to bound the reply. Absent
    `limit` ⇒ everything and `next:null`, so the default is unchanged and the
    field is inert until asked for. The cursor names the last item handed
    over, never an offset; over a live tree it may miss an item deleted
    mid-walk — this is a kernel, not a snapshot store. A cursor naming
    something gone returns EMPTY rather than silently restarting from the top.
  - `cancel task_id=<id>` → kills that task **AND ITS SUBTREE** (children
    first, like a process group) → `{cancelled:true, killed:[id,...]}`; an
    unknown/finished id → `{cancelled:false, killed:[]}` (no pretending).
    The cancelled dispatch's blocked caller sees its batch item settle as
    `{status:'cancelled', result:{cancelled:true}}` — cancellation never raises
    out of `send`.
  Advertised on EVERY reflect as `kernel_verbs` (answered by the root alone —
  advertised widely because root-only advertisement was tried and reverted: a
  model that never reflects the root never learns they exist). Bundles that fork work
  OUTLIVING a dispatch (an `exec_bridge` job) register it with
  `kernel.ctx.spawn_task(...)`, so `ps` sees it and `cancel` reaches it.
- **`on_cancel` bundle hook** — the SIGTERM half of `cancel`: cancelling an
  `asyncio.Task` frees NO real resource, and it cannot TELL anyone the work
  ended. Before aborting a task, the substrate awaits the owning agent's hook —
  if the `handler_module` exposes `async def on_cancel(id, task_id, kernel)`, it
  runs (same discovery + optionality as `on_delete`). Two things live there:
  - releasing a REAL resource — `exec_bridge.on_cancel` SIGKILLs the job's
    child process, so an aborted task cannot orphan it.
  - closing a turn out for a CLIENT — the AI backends (ollama / nvidia_nim /
    anthropic) all re-export `ai_core`'s shared `on_cancel`: it clears the
    in-flight generation (else `status`/`reflect` report a phantom turn forever)
    and emits `status(phase='done', reason='cancelled')` + `done`. The abort
    itself already drops the provider's HTTP stream and the FIFO lock.
  There is NO bundle-private `interrupt` verb anywhere: an in-flight AI `send`
  IS a kernel task, cancelled with `ps` → `cancel` like any other dispatch. (The
  `interrupt` on `exec_bridge` is a DIFFERENT thing — SIGINT to one of ITS
  jobs by `job_id`, the signal-precise twin of `stop`'s SIGKILL; not a call
  cancellation.) Pure-async bundles need nothing.
- **The CALL GATE — fully-gated, two-sided, from-above-only.** Every
  `Agent.send` passes a gate in `kernel/_authz.py` (`authorize_call` → the one
  `Decision`; io_bridge imports that type UP — the kernel owns it). The trust
  boundary is WHO ORIGINATED the send (`_current_sender`):
  - **Operator send** — `sender is None` (the CLI, the cold bootstrap, a loader
    hydrating at boot, a test driver, or a verb completing its own side effect
    under `operator_context`). The origin root of trust → **ungated**; it never
    reaches the gate.
  - **Agent send** — `sender` is set. Checked TWICE: **egress** — the SENDER's
    `can_call` must allow `(target, verb)` — AND **ingress** — the TARGET's
    `callable_by` must allow `(sender, verb)`. An edge exists only if BOTH ends
    grant it. A denial names the `side` (`egress`|`ingress`). Both fields are grant
    maps of the same shape, `{ id|token : ["<verb>", …] | "*" }`.
  The gate NEVER branches on a verb name — **nothing is exempt, `reflect` included**
  (gating reflect closes lateral discovery; an operator-conferred toolset that
  needs to see peers must grant `reflect` on them). Relative tokens resolve at the
  gate relative to the grant's HOLDER: `$self`, `$parent`, `$children` (direct
  children — set membership), `$loader` (the persistence root; rarely named, NOT in
  the baseline). The **structural baseline** the kernel stamps + MERGES on every
  agent (root, created, disk-loaded alike, in `Agent.__init__`), on BOTH fields, is
  `{$self:*, $parent:*, $children:*}` — reach/accept your own subtree and your
  creator. An explicit grant in the create payload ADDS edges on top (merge, never
  replace). **PRIVILEGED verbs** (`create_agent`/`delete_agent`/`update_agent`) are
  never covered by a wildcard — named explicitly — EXCEPT when the other end is the
  holder itself (forking your OWN subtree is baseline). This is an EGRESS rule (it
  constrains who may REACH a mutation); on ingress a `$children:*` callee
  legitimately accepts a privileged verb, since egress already guards escalation.
  **Authority flows from ABOVE only:** grants are set by the creator/operator at
  `create_agent`, or a parent DELEGATES a subset of its OWN `can_call` to a
  descendant (`callable_by` is freely conferrable — ingress grants no reach). An
  agent can NEVER raise its own authority — an `update_agent` on itself editing
  `can_call`/`callable_by` is REFUSED (`side:"self-grant"`); over-delegating more
  `can_call` than the granter holds is refused (`side:"delegation"`).
  **Persist is processed from ABOVE** (NOT a verb exemption): a consumer's
  `persist_blob`/`load_blob` to the loader, and `create_agent`'s child-boot, run
  under `operator_context()` — bookkeeping, ungated by ORIGIN.
  **Wiring pairs with granting on BOTH ends** — an operator who wires an egress (a
  scheduler `target`, an AI's `cli`/peer listener, a bundle's `file_bridge_id`
  store) grants that edge in the caller's `can_call` AND opens the callee's
  `callable_by`. An INBOUND relay (`web_ws`/`web_rest`/`ws_bridge`/`relay_connector`)
  composes the gates: `ingress_rule` says WHO may call in, its own `can_call` says
  WHERE those calls land, and the local target's `callable_by` must accept the relay.
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
  composes naturally with an ssh tunnel (the tunnel
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
  kernel-level tests live in `tests/kernel/`. `conftest.py` at root exposes
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
  records, `messages.jsonl` (the kernel's message log, appended by the
  loader's pump, hydrated cold at boot), `ledger.json`, per-agent
  sidecars (schedules.json), `lock.json`, `readme.md`.
  Wipe-and-rebuild safe.
- Use a `file_bridge` agent (rooted INSIDE the running dir — the clamp;
  created OPEN with `ingress_rule=allow_all_ingress`) for HTTP-served content:
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
   **wires + opens** that provider on demand (`create_agent parent_id=<owner> file_bridge … ingress_rule=…`,
   then set `file_bridge_id`). So the *access control* is real for in-process bundles too —
   not just the clamp. `text_file` / `scheduler` / `ai_core` / `nvidia_nim` already route
   this way; writes fail loud when no surface is wired (no silent RAM).

The ONLY code that imports `fs` directly is **the file_bridge agent itself** (it IS the
gate) and **the cold bootstrap** — the bring-up *before* any agent/gate exists, not an
exception to access control: substrate fixed-path reads (`kernel/_lock.py`, `_env`,
`Agent._read_readme`; the substrate is decoupled from bundles and cannot import one) +
`kernel_state`'s boot-read of `.fantastic` (`read_tree`, the chicken-egg root hydration).
*In flight toward the end-state:* `terminal` still imports `fs`
(`external=True`) because it reaches OUTSIDE cwd (/tmp scratch) — it folds into
the agent model once a file_bridge agent can carry an explicit external root.
(`local_runner`/`ssh_runner` are DELETED — sub-kernel reach is the ssh connector +
`exec_bridge`.) (`web` is
already clean: `/` is the agent index, `/file/` proxies to gated agents, own assets are
importlib package resources — no `fs`.)

`fs`'s two surfaces, picked STATICALLY per call site (never tried in sequence — that would
be a fallback, which this kernel refuses):
- **clamped** (default) — `fs.read_text(root, path)`: `root` clamped to cwd, `path`
  clamped within `root`. This is the kernel's OWN state (its `.fantastic`). The running-dir
  law guarantees the kernel can't escape its own dir.
- **external** (`fs.read_text(root, path, external=True)`) — `root` is an operator/peer
  base that may live ANYWHERE; `path` still clamped within it. For surfaces that operate
  OUTSIDE cwd BY DESIGN: `terminal`'s clipboard-image temp scratch dir.

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
