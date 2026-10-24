# Fantastic Kernel — Claude Code working notes

A medium that unifies humans and AIs into a single workspace. Recursive
`Agent` class + a `Kernel` shared-context object, one primitive — the ARRAY-ONLY
batch send `send([{target, payload, timeout?, id?, retries?}, ...]) -> [{pid, status, result}]`
(single call = `(await send([{...}]))[0]`; statuses
`completed|errored|failed|cancelled|timed_out`; concurrent dispatch, input order
preserved; timeout per item, NEVER defaulted by the kernel, enforced through
`cancel_task` so `on_cancel` hooks run; there is deliberately NO single-call
sugar — one shape prevents drift) — plugin-discovered bundles. Every agent
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
│   KERNEL verbs (create/delete/update/list_agents + ps/cancel + docs) —   │
│   the ROOT answers them; `docs of=kernel` lists them.                    │
│   into Agent.                                                            │
│   CALL GATE — two-sided deny-all `Agent.send` (kernel/_authz.py): an     │
│   AGENT send needs the sender's `can_call` (egress) AND the target's     │
│   `callable_by` (ingress) — reflect included; OPERATOR (sender=None)      │
│   ungated. Baseline {$self:*}; grants are the only rule (tree verbs too). │
│   Persistence DECOUPLED — `kernel_state` (root) is a weak-bound STREAM      │
│   CONSUMER: it auto-persists records THROUGH a `file_bridge` provider's     │
│   write_stream (ephemeral `kernel_store`); cold read stays direct.          │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (agent.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌──────────┐    ┌────────────┐              ┌────────────────┐
   │ kernel_state│    │ web        │              │  exec_bridge   │
   │  (ROOT)  │    │ (uvicorn)  │              │ jobs · PTYs    │
   │ cli·file │    │ HTTP + WS  │              │ ai · ...       │
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
LLM CLI) bootstraps from a single
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
the frontend. Declared-config metas the substrate wires (not behavior): `root`
(a loader serves a sub-namespace),
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
  compute job), an AI backend (one inference pass), and a `file_bridge` (durable
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
  recalls them on a FRESH history-less message, updates them, and prunes precisely
  — managing durable memory through `send` alone; the which-verb/when is
  derived, not coded. Memory is plain FILES through a `file_bridge`'s text
  verbs; the judgment claim validates against them in e2e. Only the Swift FM
  backend auto-injects memory each message; the Python backends reach it on
  demand by id.

## Run

```bash
uv sync                                              # install workspace + bundles editable
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default: kernel)
fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=8888   # persist web record
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
fantastic kernel create_agent parent_id=<web> handler_module=web_ws.tools ingress_rule=allow_all_ingress
# shared group — require the group token (from $FANTASTIC_GROUP_TOKEN):
fantastic kernel create_agent parent_id=<web> handler_module=web_rest.tools ingress_rule=password_ingress
```

Without the `ingress_rule`, the leg mounts but every inbound frame/request is
denied (`{reason:"unauthorized"}`) — opening is a deliberate act
(G2: sealed by default). Same for a bridge leg (`ws_bridge`/`relay_connector`): set
`ingress_rule` to admit inbound calls.

**Octet serving** — `web` bakes in `GET /<agent_id>/file/<path>` (see `app.py`):
any agent answering the SOURCE stream verb `read_stream` becomes an HTTP file
server, so a LARGE file pipes out chunk-by-chunk;
the **allowance is the served agent's own gate** (a sealed `file_bridge` denies →
`404`), path clamped to its root, and only what the bridge's `allow` list exposes is
served (anything else → `404`). The PUMP (`file_bridge.pump`) is the server-side
SOURCE→SINK copy; both ends run AS THE FILE_BRIDGE (its own sends) — the bridge
needs `read_stream` on the source and `write_stream` on the sink, both ends granted.

The bootstrap wires the stdout renderer (Cli) when stdin is a tty —
ephemeral, never persisted.

**First contact (tty / PTY).** On a tty — a human, or an LLM dropped into an
`exec_bridge` PTY job — the daemon greets whoever lands there (the tty twin of
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
imports the bundle). Rust carries no stdout-renderer bundle: its
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
| `static_loader` | a DECLARATIVE roster loader (`loader/static_loader`). Reads a `fantastic.yaml` manifest (`manifest` field) and at `boot` hydrates the declared agents — kernel bookkeeping: each a direct call of the kernel's own `create_agent` (`Kernel.bookkeeping` on the root), never a gated send — as its own children — each carrying the baseline grant + the manifest's explicit `can_call`. The kernel never parses YAML; this bundle owns the format (mirror of `kernel_state` owning the `.fantastic` layout). That pass runs only on the kernel's own boot (origin none: the cold boot, `create_agent` booting the loader); an agent-sent `boot` is refused — `{reason:"unauthorized", booted:false}`, "static_loader: an agent-sent boot is refused — hydrating the roster is the kernel's own bookkeeping, so only the kernel's own boot (cold boot, create_agent) runs it". After boot the roster is CLOSED by deny-all (an agent creates only its OWN children, and only when the root's `callable_by` and its own `can_call` grant `create_agent`), so the manifest IS the roster. Idempotent by id (a reboot re-reads + skips existing). Seed with `fantastic init static_loader ./fantastic.yaml`. |
| `cli` | singleton child of root; renders token/done/say/error events to stdout |
| `web` | uvicorn HTTP host. Renders nothing of its own (an undeclared `/` is a 404) — bakes in only `/<id>/file/<path>` (the `read_stream` file proxy) and the favicon. Call surfaces (WS, REST) live in sibling sub-agents: they DECLARE routes via the duck-typed `get_routes` verb (pure JSON data — `{kind, method?, path}`, so the reply renders over REST and persists in the ledger) and hand over the live endpoint per path through the duck-typed `route_endpoints(agent)` module hook (discovered like `on_delete`); a leg declaring a route without the hook fails web boot. Listens on `bind` (record meta; else `host`, else `127.0.0.1`; set `bind: 0.0.0.0` to publish, e.g. a container: every address is taken and the guard keeps its loopback identity, so `localhost:<port>` is admitted). The guard judges by `host` (record meta; absent ⇒ `127.0.0.1`) and refuses EVERY request (GET and the websocket upgrade included) whose `Host` is not its own authority — `<host>:<port>`, plus `localhost`/`127.0.0.1`/`[::1]` on that port when `host` is loopback; a `host` of `0.0.0.0` answers only `0.0.0.0:<port>`, so a LAN name / tunnel name is refused, and `bind` alone never changes the guard's answers (403, logged; the DNS-rebinding read); refuses websocket upgrades and non-GET/HEAD/OPTIONS requests from a browser `Origin` that is neither its own nor listed in `allowed_origins` (403, logged; no Origin ⇒ allowed). The TS frontend kernel brings its own typed WS bridge (`ts/`) — no transport script is injected here. |
| `web_ws` | WebSocket verb-invocation surface — the **ws INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `/<host_id>/ws` on its parent web's FastAPI app. Gates every inbound frame at the shared `gate_inbound` choke point with the leg's own `ingress_rule` (credential on the frame envelope `auth_token`); `egress_rule`/`auth` per leg as on a bridge. **SEALED BY DEFAULT** — a freshly-created `web_ws` with no rule denies (a browser can't connect until opened); open it consciously (see "Run"). Opt-in: `create_agent handler_module=web_ws.tools parent_id=<web> ingress_rule=allow_all_ingress`. |
| `web_rest` | REST diagnostic surface — the **http INBOUND derivation of `io_bridge`**. Child of a `web` agent. Mounts `POST /<self_id>/<target_id>` body=payload → kernel.send → JSON reply (+ `GET /_reflect` shortcuts). Gates each request via the leg's `ingress_rule` at `gate_inbound` — FIRST, on the target and credential alone, before one body byte is read; an admitted body is read up to 16 MiB (`MAX_BODY` = io_bridge `MAX_FRAME`), larger → `413`; the credential rides the **`X-Fantastic-Auth` header** (http carrier, not the envelope); the item's `id` rides `X-Fantastic-Execution-Id`. An admitted request is handed to the leg (`inbound_call`, as the transport — the operator) and the leg sends it AS ITSELF. **SEALED BY DEFAULT** — absent rule ⇒ deny (`403 {reason:"unauthorized"}`); open with `ingress_rule=allow_all_ingress`/`password`. Multiple instances coexist with different ids. Opt-in. |
| `io/file_bridge` | filesystem-as-agent — text documents (`read` line-numbered + `version`, atomic `write`, SEARCH/REPLACE `edit` as a transaction over files, `append`, `if_match` guard, `undo` — one RAM slot per path, lost on restart), streams (`read_stream`, `write_stream`, `pump`), `list`, `delete`, `rename`, `mkdir`, `watch_path`/`unwatch_path` (a disk watcher — `watchfiles`/`notify`/DispatchSource — on an exposed file or directory, held per caller in RAM; every change by any writer emits `{type:changed, path, kind}` on the bridge's own inbox for whoever WATCHES it; a file is watched through its parent too, so an atomic save is seen) — the **fs edge of the io family**: crossing kernel↔disk is gated like any io_bridge leg (same `ingress_rule`/`egress_rule`/`auth` fields + registries), **SEALED BY DEFAULT** (absent rule ⇒ every verb except `reflect` denies `{reason:"unauthorized"}`; open with `ingress_rule=allow_all_ingress`), bound by the **running-dir law**: root + every path are clamped inside the dir the kernel runs in (`../`, `~`, absolute escapes, outward symlinks refuse), and **DENY-ALL AT THE PATH LEVEL**: the record's `allow` list names what it exposes (a file, or everything under a directory; `.` = the whole root) — no list exposes NOTHING; every path verb sees only exposed paths, anything else is `not_found` whether or not it exists; set at create or by `update_agent` (a record field); `readonly` still governs writes |
| `io/exec_bridge` | commands-as-agents (`start`, `status`, `stop`, `interrupt`, `clear`, `tail`, `grep`, `boot` — `tail`/`grep` read a job's output SO FAR, running or ended, from its live 256 KiB line ring) — the **process edge of the io family**: crossing kernel↔subprocess is gated like any io_bridge leg (same `ingress_rule`/`egress_rule`/`auth` fields + registries), **SEALED BY DEFAULT** (absent rule ⇒ every verb except `reflect` denies `{reason:"unauthorized"}`; open with `ingress_rule=allow_all_ingress`), and bound by a **regex command ALLOWLIST**: `allowed` on the record is a list of regexes, and `start {command, args}` refuses unless the shell-joined line fullmatches one — enforced through `proc`, the ONE process surface, exactly as `fs` is the one opener. No per-call override, no default, no PATH fallback; an empty `allowed` runs nothing. WHERE and WITH WHAT a command runs is the record's too: `cwd` (REQUIRED — a leg without one runs nothing), `env` (operator extras), `inherit_env` (which kernel env vars pass through — none by default, `["*"]` = all); a per-call `env`/`cwd` is refused; a bare `command` is found on the CHILD's `PATH` only (none → refused naming `PATH`). **A terminal is a PTY JOB**: `start {command, args, pty:{cols,rows}}` runs the command on a pseudo-terminal behind the same gate (with no `command`, the CHILD's own `$SHELL` from `inherit_env`/`env`, permitted by the record's `shell: true|"login"` as exactly `$SHELL` / `$SHELL -l`); it streams `output {job_id, data}` (incremental UTF-8) and is driven with `write` (keystrokes, full + serialized — bracketed-paste-safe), `resize`, `ack` (VSCode flow control: pause past 100K unacked chars, resume under 5K), `output {job_id}` (the 256 KiB ring, for re-attach) and `paste_image` (image → scratch file → path typed, for a CLI like `claude` in a browser xterm) — each a gated verb, so a viewer can be read-only. Any caller the gate admits drives any job of the agent; the owner-inbox relay drops a drive verb only when its `job_id` names ANOTHER job — another job's keystrokes, pastes and drive calls never reach a running program's `on_message`, while a drive verb with no `job_id` (the bulk `stop`/`interrupt`/`clear`) or naming this job is forwarded (`DRIVE_VERBS`). Nothing respawns a terminal: `boot` starts nothing; a client that attaches finds no live job and `start`s one itself. It is a GENERAL-PURPOSE COMMAND runner. A durable program is a FILE some process (or a `write_stream`) put on disk; running it is `command` + `args` like anything else, so the job table / output pump / connector relay / fd passing / task-table rows / mail contract are all language-blind. A spawned program speaks the kernel over the inherited `FANTASTIC_CTRL_FD` (`[4-byte BE len | JSON]`); to get a `kernel` global it includes the reference wire client (`exec_bridge/_connector.py`, `CONNECTOR_SRC`) in its own source — the bridge prepends nothing. A call is `{op:"send", rid, target, payload}` (`kernel.send(target, payload=None)`); the relay is a task the runner's `start` spawned, so a program's sends are the RUNNER's, gated as it. Any verb the bundle does not own is MAIL for the running program (`{queued:true}`); jobs are kernel TASKS — a job's `job_id` IS its task id, an int from the one counter (`ps` lists it, `cancel task_id=<job_id>` kills it; `on_cancel` SIGKILLs the real process; a string id is refused, never coerced); `boot` is a no-op (nothing auto-restarts — a command is started by a caller, not the record). NOT a sandbox: an allowlisted command is a NAMED command, with all of that command's reach. |
| `io/py_agent` | the BASE a FOLDER BUNDLE with a program extends. A folder `<root>/bundles/<name>/bundle.yaml` (`base: py_agent`, `sentence`, `command` — argv, cwd = the folder, `verbs` — verb → doc, the ONE truth `docs` and `reflect bundles=all` read, optional `emits`/`notes`/`record` defaults) is read by the root loader at boot (before `kernel.load`) and on the root's `reload_bundle {name}` (a failed reload keeps the previous load running); any loader's `read_bundles {name?}` hands its own root's `bundle.yaml` files RAW for a kernel that loads them itself (the ts page kernel parses them); an instance is `create_agent handler_module=<name>`. The kernel runs the instance's program (env `FANTASTIC_CTRL_FD`, `FANTASTIC_SELF_ID`, the record's `env`/`inherit_env`) when it boots and kills it on delete/stop; callers reach it ONLY through its verbs — no process verbs. Wire = the exec_bridge connector channel (one relay, `_serve_ctrl(ops=, on_push=)`): child `hello {verbs}` first (must equal bundle.yaml, else the instance stops saying so), kernel `call {rid, payload}`, child `result {rid, result}`, plus `send`/`emit`/`watch`. An exit settles in-flight calls (`transport_closed`) and nothing respawns; `reload_bundle` re-reads the folder and restarts live instances abruptly. A folder named like a built-in, with an unknown base or a bad field is refused and listed with its `error`. stdout/stderr are drained (the last 2000 chars of stderr show as reflect `stderr_tail`). Reference child: `py_agent/fantastic_agent.py` (stdlib). SECURITY: the program runs with the user's OS rights. Creating `py_agent.tools` itself is refused (a base). |
| `io/mcp_client` | ONE remote MCP server (Streamable HTTP, protocol **2026-07-28 only** — stateless, `_meta` on every request; an older server is refused naming its versions) as an agent. Record: `url` (the MCP endpoint, required) and `credentials` ({store, ref} holding `api_key` — a STATIC key, sent as `Authorization: Bearer` to the ref's host only, read per request as an AI backend reads its key; absent = keyless; no OAuth). `boot` runs `server/discover` + every page of `tools/list`; EACH TOOL IS A VERB of this agent (`send {type:<tool>, …args}` = `tools/call`, with `Mcp-Name` / `Mcp-Param-*` headers); `docs of=<agent>` serves the agent's OWN table (`instance_verbs`, built from each tool's schema); a tool named like a kernel/lifecycle verb, listed twice, or with an illegal `x-mcp-header` is skipped and named in reflect `skipped`. Answer: `{content, structured?}` — `isError` adds `error` (settles errored); image/audio `data` and a resource `blob` decode to RAW BYTES — ONE body per reply at its top-level `bytes` (the item says `body: "bytes"`), a later one only `size` + `omitted`; `input_required` answers `{error, input_required}`; protocol errors map to the closed refusal set. Emits `tools_listed`, `progress`, `log`. A failed re-list keeps the tools already listed. A grant names tools like verbs. |
| `cabinet` | a master AI's cabinets (#132): a manager (`cabinet_role: manager`, `canvas_id` = the spawning canvas) opens cabinets — each its OWN CANVAS, a CHILD of that canvas (`new_canvas`, nested in the tree), with a live html status cell (`cabinet_role: cabinet`, whose button opens the canvas) and ONE worker AI on an existing backend by default. The worker builds its equipment on its cabinet's canvas (`create`, `place` only there; an AI made there reaches its store). Every open/close rewrites the workers' peer directory and grants; `conclude {keep}` keeps cabinets or results (agents on a cabinet's canvas) and closes the rest. Readme: `src/cabinet/readme.md`. Python is the reference; Rust is ported (`fantastic-cabinet`, the same canvas design), Swift is not yet
| `scheduler` | recurring tasks; its schedules persist through the loader (`persist_blob`/`load_blob`). **A schedule fires AS THE SCHEDULER** — one batch under its own id, so the scheduler's `can_call` and each target's `callable_by` decide; `list`/`history`/`tick_now`/`unschedule`/`pause`·`resume {schedule_id}` see every schedule. The tick loop is a task the scheduler's `boot` spawned, so its fires are the scheduler's own sends; its sidecars persist by DIRECT calls of the loader's `persist_blob`/`load_blob` functions (bookkeeping, not a send) |
| `memory/sqlite_store` | the DURABLE memory store: ram_store's verbs, replies, refusals, tables and cascades over ONE SQLite file (`path` on the record, relative to the running dir, REQUIRED; stdlib `sqlite3`; WAL, `synchronous=NORMAL`, busy timeout, FKs `ON DELETE CASCADE`, an FTS5 trigram index for `search`) — a caller cannot tell the two apart except `durable: true`. Rows, folds, the ledger, dead letters and named-call dedup survive a restart under the same guid, no dump and inject; a `ram_store` dump injects here and back. `delete_agent` deletes the file with its WAL/SHM; a kernel stop (`on_shutdown`, after the final flush) checkpoints the WAL and keeps it. Same schema as the rust CLI's `sqlite_store`; both runtimes replay `contract/memory_store/cases.json` against both stores. |
| `memory/ram_store` | the MEMORY STORE in RAM (ephemeral — gone with the process), with the topology of a SQL store: rows, fold refs (cascade on prune), a text index. Multi-tenant: every row under `<guid>|<agent id>` (the operator: `<guid>|kernel`); a caller reads/writes only its own key. Verbs `append`/`read`/`search`/`prune` (own rows), `inject`/`dump` (the whole tables — plain verbs the gate decides; mock rehydration). The root's `memory_store_id` and every AI agent's name one. |
| `ai/ollama/ollama_backend` | local LLM agent (ollama); `endpoint` required, `model` left out is READ FROM THE SERVER before a turn (the ONE model `GET /api/tags` lists) and written back; optional `credentials` (Bearer) for a server behind a door; per-client chat threads, FIFO lock, menu cache; RAW prompt-and-parse tool-calling — the `<call name=…>` TAG grammar with one child tag per argument and a verbatim `<bytes>` body, NO JSON on the model path (see `ai_core.tool_parse`) |
| `ai/responses/responses_backend` (`responses_backend.tools`) | LLM agent over the OpenAI **Responses API** (`POST …/v1/responses`) — OpenAI, Ollama ≥ 0.13.4, or a proxy in front of an open-weights model. `endpoint` (the full URL), `model` and `credentials` (the key's `credential_store` ref) REQUIRED on the record, nothing guessed. Stateless (`store:false`, the whole conversation as typed `input` items, the system block as `instructions`). Reads BOTH channels — `reasoning_text`/`reasoning_summary_text` deltas wrapped in `<think>`, `output_text` deltas as output; a native `function_call` item is a loud error. A `max_output_tokens` cut (`response.incomplete`) fails a main turn and is an answer (`last_stop_reason:"max_tokens"`) on a bounded side call. Kept SEPARATE from the Chat-Completions backend (`vllm_backend`): a different request shape and event stream |
| `ai/vllm/vllm_backend` (`vllm_backend.tools`) | LLM agent talking to a **vLLM** server DIRECTLY (`POST {endpoint}/chat/completions`, no proxy). REQUIRED on the record, refused AT THE WRITE when any is missing: `endpoint`, `model`, `max_tokens`, `kv_slots`, `kv_overflow`, `context_window` (the server's window, used as set: a main turn whose `max_tokens` reaches it sends NO `max_tokens` — the server bounds the reply by what the prompt leaves; a side call keeps its bound). Reads the thought channel (`reasoning_content`/`reasoning` → `<think>`), merges every system message into one at index 0 (Qwen's template refuses others), treats a stream ending without `finish_reason` as vLLM dying (an error), a `length` finish as a cut. Sampling defaults where the record sets none: temperature 0.2, top_p 0.8, top_k 20, reasoning_effort medium (`low`\|`medium`\|`xhigh`), thinking_token_budget 16000 (must stay below `max_tokens` — refused at write; a main turn sends an explicit budget as set, the default as `min(16000, max_tokens//2)`; a bounded side call (a fold summary) gets `min(budget, its max_tokens//2)`; under 256 NO budget is sent — a starved thought ends in an empty answer). **KV-cache session table**: ONE per endpoint, shared by every vllm_backend agent in the kernel — `kv_slots` live conversations (identity = system text + first user turn, i.e. the cached prefix), `kv_overflow` `evict` (LRU rotates out) or `reject` (newcomer refused with a retry time), `kv_ttl` 1800 s; bookkeeping only (vLLM owns the cache), shown in reflect `kv`. Timeouts connect 5 s / read 900 s; each failure says what it means (refused = vLLM still loading, connect timeout = instance stopped, 401/403 = name the key in `credentials`). **Key**: the record's optional `credentials` ref, read per request and sent as `Authorization: Bearer` — absent sends no header, so a keyless local vLLM works. Run the server WITHOUT a tool parser |
| `ai/anthropic/anthropic_backend` (`anthropic_backend.tools`) | LLM agent over Anthropic's **Messages API** (`POST {endpoint}`, streamed SSE). REQUIRED: `endpoint` (the Messages URL, e.g. `https://api.anthropic.com/v1/messages`) and `model`; `max_tokens` / `context_window` left out are READ FROM THE API before a turn (`GET …/models/{model}`: `max_tokens`, `max_input_tokens`) and written back. RAW tool-calling (no `tools` array); one top-level `system`, consecutive same-role turns merged; `thinking_budget` asks for extended thinking (`thinking_delta` → `<think>`); a stream without `message_stop` is an error, an `error` event carries the API's message. **Key** optional (`credentials`, sent as `x-api-key` with `anthropic-version`); absent, no key header |
| `security/credential_store` | SECRETS BY REF — a credential is a ref (`openai/main`) holding named fields, bound to ONE host. Verbs `set {ref, fields}` (takes NO host — a `host` on the call is refused, "credential_store set: takes no host — a ref's host is its declaration, written by update_agent on this store's refs"), `get {ref, host?}` (`host` refuses a ref bound elsewhere), `list` (metadata only), `forget {ref}` (drops the VALUE and its field names; the declaration stays; refused on the read-only `env` backend). A `set` sent with `secret: ["fields.<name>"]` is recorded with those values shown as `[secret]`; a `get` reply is recorded like any call's, in clear. Backends (`backend`): `file` (default) — an ABSOLUTE path, `<home>/user/credentials/<id>.yaml` (`<home>` is `FANTASTIC_HOME`, else `~/.fantastic`) unless `file` names another (`/…` or `~…`; a relative one is refused at the write: "file='<p>' must be an absolute path (starting with / or ~) — a relative path is refused"), written 0600, read only if a regular file owned by this user with no group/other bits — judged on the ONE open it is read from (`fs.read_private`: `O_NOFOLLOW`, then `fstat` of that descriptor); `keychain` — the OS keychain, chosen explicitly (`keyring`), items under `keychain_namespace` (default `fantastic`; macOS's item "service"); `env` — read-only, `refs.<ref>.env` maps fields to variables. The bound host is stored WITH the value: the file entry and the keychain item are `{host, fields}`, the host written by `set` from the declaration; a `get` whose declaration names another host than the stored one is refused, and so is a stored value with no host — a store elsewhere declaring the same id and ref reads nothing (`env` keeps no value at rest). A ref is DECLARED (and undeclared) only by `update_agent` on the store's record — `refs: {<ref>: {host, fields?/env?}}`, `host` required: `set` stores a value for a declared ref under the DECLARED host and never creates one — an undeclared ref is "no such ref", the operator included; `forget` never touches the declaration. There are no per-ref access lists: any caller the gate admits may `set`/`get`/`forget` a declared ref. The record holds the host and field NAMES, never a value. Every AI backend reads its key here: `credentials: {store, ref}` on its record, read AS the agent before each request (`ai_core/credentials.py` — the request runs in the agent's own turn) — nothing reads a key from the environment. |
| `io/io_bridge` | the **IO base — pure shared library, NOT a registered bundle/agent** (no entry point, never instantiated). Holds the channel model (`direction · modality · transport · rule · extractor`), the `ingress_rules`/`egress_rules` registries, the transport-agnostic bridge engine (read loop, the 6 verbs, lifecycle, `make_verbs` factory), the unified `gate_inbound`/`stamp_egress` choke point, and the in-process `MemoryTransport`. Every derivation (`ws_bridge`/`relay_connector`/`web_ws`/`web_rest`/`file_bridge`) **imports** from it and reuses `rule.authorize`. IO legs are **SEALED BY DEFAULT** (`resolve_ingress({})` ⇒ `DenyInbound`); the open interior is unaffected. EVERY leg — the engine's wire bridges included — resolves its `ingress_rule`/`egress_rule`/`auth` off the LIVE record per frame (never a boot-time copy), so a seal or an opening by `update_agent` holds from the next frame and `reflect` always shows what the gate enforces. A `forward`'s frame id is `<leg id>-<boot nonce>-<n>` (12 hex, minted at every boot): the receiver names the execution by it, so a restarted sender never joins an execution it already settled. A denied client is taught by the denial's `hint` (the inline recipe — `update_agent <id> ingress_rule=allow_all_ingress`) and `reflect readme=true` on the **sealed agent itself** (each derivation ships its own short readme). |
| `io/ws_bridge` | cross-kernel comms — a thin **WS-only, asymmetric** transport **derivation of `io_bridge`** A bridge agent opens a WS to the remote's `web_ws` surface and ships raw `{type:"call", target, payload}` frames; the remote dispatches `kernel.send` exactly like a browser frame and replies over the same socket — **no peer bridge needed**. Transports: memory (test backbone) / ws (`host` explicit) — it spawns nothing; an ssh tunnel is an `exec_bridge` job or devops. Streaming via `watch_remote` (`{type:"watch", src}` out, `{type:"event", src, payload}` back, re-emitted on the bridge's own inbox as payload + `src` ONLY for a subscribed `src` — nothing flows before `watch_remote` or after `unwatch_remote`). **Inbound calls are the LEG's**: a leg serving inbound calls (paired transport, relay) sends each one AS ITSELF (its read loop is a task its `boot` spawned) — the leg's `can_call` and the target's `callable_by` decide; whoever called on the far kernel is not an identity here. All transports are **weak binding** — addressed by URL + path only; no shared Python types with the remote kernel. Weak proxy: local→local stays direct. **Authorization**: two symmetric typed per-leg rules (enforced on the receiver) — `ingress_rule` (inbound FILTER, gated at the shared `gate_inbound` choke point) + `egress_rule` (outbound DECORATOR, stamps a credential on the frame envelope); `auth` is a shorthand for both. **SEALED BY DEFAULT** — absent rule ⇒ `deny_inbound` (reply `{reason:"unauthorized"}`); open consciously with `ingress_rule=allow_all_ingress`/`password` (kernel-GROUP shared secret from an env var). Resolved by name from the `io_bridge.ingress_rules` / `egress_rules` registries; shared with relay_connector. |
| `io/relay_connector` | cross-kernel comms through a **relay-KERNEL router** (`../fantastic_relay`). Same verbs/frames as `ws_bridge` (both ride the shared `io/io_bridge` engine). A connector dials the relay at `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, group password in the `X-Fantastic-Auth` header, checked once at the WS upgrade — the password lives in a `credential_store`, NEVER on the record: `credentials: {store, ref}`, read before every dial AS the connector (the dial runs in its dispatch or a task it spawned), field `password`, pinned to the relay URL's host, as an AI backend reads its key; no `credentials` ⇒ boot fails loud) and reaches a partner kernel by its `partner_guid`; the relay routes by `target` and delivers peer→peer ONE-WAY as `{type:"event", source, payload}`. The connector **TUNNELS** the bridge frames inside relay `{type:"send", target:partner, payload:<frame>}` (out) / unwraps partner `event.payload` (in), so forward/reply correlation + symmetric serving work UNCHANGED — both peers run a connector. Raw `read_stream` chunks ride as native binary WS frames (no base64; the relay forwards the frame kind). **No certs / mTLS / token-issuer** — the relay auths the connection via the header password and routes by GUID. Self-healing: auto-connect + auto-reconnect (`reconnect` meta, default 10s; `0`=one-shot), `keepalive` every `heartbeat`s (default 30), `reflect.connected`=live socket. Directory passthrough (the relay's own `relay` agent, `target:"relay"`): `list_peers` → `{peers:[{guid,status,last_seen,since}]}`, `watch_directory`/`unwatch_directory` re-emit `peer_joined`/`peer_left`/`peer_evicted`/`peer_status` on the connector's inbox. All three host runtimes (python/rust/swift) interop any-to-any — proven by `integration_tests/relay_e2e` against a live `relayd`. Our own `guid` (the WS path) **auto-mints on first boot if absent and persists** into the record (explicit `guid` wins, a minted one is never regenerated — stable across reboots); `partner_guid` stays explicit (discovered via the partner's `reflect`/`list_peers`). **Directory typing (kernelgroup):** a connector advertises an opaque **attrs blob** to the relay on connect (re-announced each reconnect) — well-known keys `role` (`manager`\|`kernel`, default kernel), `owner_guid` (managing peer, null=standalone), `exposes` (control-surface list); the relay stores + reflects it (into `list_peers` entries' `attrs` + a `peer_updated` event) and NEVER interprets it. `set_identity` (verb: optional role/owner_guid/exposes, merged) updates live + persists. reach ≠ control — driving another manager's kernel is a plain `forward` to that manager. Record fields: `relay_url` · `guid` (auto-minted if absent) · `partner_guid` · `credentials` ({store, ref} — the relay password, X-Fantastic-Auth) · `heartbeat` · `reconnect` · `role` · `owner_guid` · `exposes`; `transport="relay"`. Per-leg `ingress_rule`/`egress_rule` (or the `auth` shorthand) gate the **tunneled** calls and are INDEPENDENT of the relay connection auth — directional/hub-spoke topology + kernel-group `password` membership (shared secret from env); IO legs are **SEALED BY DEFAULT** (absent rule ⇒ `deny_inbound`), opened consciously (`ingress_rule=allow_all_ingress`/`password`). |

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

A record's `handler_module` resolves ONLY from that catalog
(`kernel/_bundles.import_handler`), plus `register_unlisted(name, module)` — a
test/probe module that dispatches but is never listed (rust's
`BundleRegistry::register_unlisted`); any other name is never imported, even
when it is on `sys.path`. Such a record is kept as one
this runtime cannot answer — a call to it is refused `unhandled`, "… cannot be
imported in this runtime: ModuleNotFoundError: No module named '<top-level
package>'", and `Kernel.load` skips it — exactly as rust and swift treat a name
they did not register.

## Universal patterns

- **`reflect`** — every agent answers `{type:"reflect"}` returning the
  ADDRESSED agent uniformly: `{id, sentence, display_name, description?,
  ...flat state}` — what it IS, never how to call it (no
  `verbs`, `emits`, `cancel`, `more` or `kernel_verbs` on any reflect). Root is NOT
  special. Compose the reply with flags:
  `tree=all|ids|none` (default `all` — nested distilled subtree; `ids` =
  flat descendant-id index; with either, `show_subtrees=[ids]` answers only
  those branches as a list and `ignore_subtrees=[ids]` prunes each to
  `{id, pruned: n}` — show first, then ignore inside it; an id outside the
  reflected tree is `bad_arg`), `bundles=all|ids|none` (default `none` —
  the installable-bundle catalog), and `readme` — polymorphic: `true`
  attaches the agent's SHORT orientation `readme.md`; `"index"` returns
  the token-annotated topic tree `[{key,desc,tokens,children?}]`;
  `"<key>"` drills one topic (body + its sub-docs' index); an unknown key
  → `{error,keys}`. Discovery is one round-trip;
  transport/wire docs live in the doc tree (`reflect readme=index`, then
  drill by key). The ROOT reflect with `detail:"full"` carries `readmes`
  (the top-level topic index) + a `docs` one-liner; the distilled default
  leaves them out (a short `self_extension` points at the doc tree), and
  `readme:"index"` serves the map on demand.
- **SECRET MARKS** (`kernel/_secret.py`) — a send item may carry
  `secret: ["<dotted path>", …]`, paths into its payload (`"fields.api_key"`). The
  handler gets the payload as sent; the call row, the state stream (`payload`,
  `summary`) and the inbox mirrors carry it AS SHOWN — each marked value is
  `[secret]`. The marks themselves are recorded (`row.secret`). A handler reads them
  as `current_secret()` and marks its own onward send the same way (a relay). Over
  the wire: a ws `call` frame's `secret`, a REST `X-Fantastic-Secret` header
  (comma-separated); the leg's `inbound_call` row is marked under `item.payload.`.
  A malformed field is `bad_arg` `secret`; `type` cannot be marked.
- **`docs`** — HOW TO CALL an agent, a KERNEL verb answered by the root
  (`kernel/_docs.py`): `{type:"docs", of, tier?, verb?}` → `{of, bundle, tier,
  verbs, emits?, cancel?, notes?}`. `of` is `kernel` (the kernel verbs,
  `_SYSTEM_VERB_DOCS`), an agent id (the bundle it runs) or a handler module (no
  agent needed). `tier:"oneliners"` (default) = each docstring's first paragraph cut
  at its first top-level `. ` or ` — `; `"detailed"` = the whole docstring, and with
  no `verb` also the module's `EMITS` / `CANCEL` / `NOTES` (duck-typed, like
  `RECORD_SCHEMA`). Unknown `of`/`verb`/`tier` → `bad_arg`; no `of` → `missing_arg`.
  PER-INSTANCE VERBS: `of=<agent id>` whose module exposes the
  duck-typed `instance_verbs(agent)` is documented by THAT table (None keeps the
  module's) — an MCP server's tools; rust `Bundle::instance_verb_docs`, swift
  `instanceVerbDocs`.
  **Gated like every kernel verb** — a `docs` grant on the root, both halves. In `ai_core` a
  `docs` reply is cut to a one-line stub once the model has answered it
  (`drop_read_docs`), and a refusal of the model's own call (`unhandled` naming
  the verb, `missing_arg` naming the arg) gets a hint pointing at `docs`.
  Each distilled **tree node** also carries the wiring/posture meta it
  has — `ingress_rule`/`egress_rule`/`auth` (a leg's lock; names are
  MANIFOLD-SCOPED: `allow_all_ingress` = open, `password_ingress` = gated,
  `deny_ingress` or **absent on an io leg** = sealed-by-default. `auth` takes a
  bare STEM and each manifold appends its own suffix, so one name can never
  half-resolve across the two registries), `root` (a file_bridge's served dir), `allow`
  (what that bridge exposes under its root), `file_bridge_id`
  (what a consumer persists THROUGH) — so the whole IO landscape (which
  legs are open vs sealed, what's wired to what) is readable from ONE root
  reflect, no per-agent round-trips. A reflect that passes the gate returns the
  tree whole (roster and `detail:"full"`).
  The **ROOT** reflect additionally carries `runtime` — a stable lowercase
  enum (`python` | `rust` | `swift` | `ts`) naming the kernel's runtime, so
  a client gates runtime-specific UI from one round-trip. Same field name +
  values across all four runtimes; injected uniformly in
  `_apply_reflect_flags` (root only). Differs by runtime BY DESIGN (like the
  root id), so cross-runtime parity asserts the per-runtime value, not equality.
  The root reflect ALSO carries two deployment-context fields, in key order
  `runtime → env → version`: `env` (read from `$FANTASTIC_ENV`, default
  `"host"`; the container image bakes `"container"`) tells a client WHERE the
  kernel runs — e.g. `env:"container"` means `kernel_stop` stops + (under
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
  The root reflect ALSO carries `peers: [{guid, via}]` — the kernels this one is
  CONNECTED to (a browser page after its `hello` on a `web_ws` leg). **Cross-kernel
  addressing:** an agent there is `<guid>|<agent id>`, called with an ordinary `send`
  and discovered by `reflect` to `<guid>|kernel`. The KERNEL routes it (`ctx.peers`,
  registered by the transport that holds the connection — the leg only carries
  frames): the sender's `can_call` must name `<guid>|<agent>` or `<guid>|*` (egress
  is the whole host-side check), the peer's own gate judges the call as the leg
  that serves it there; `<own guid>|x` is the local `x`; a guid not connected is an
  unknown agent; a disconnect mid-call settles the call as a `transport_error`.
  **A guid holds a SET of live routes** (the tabs of one identity
  are one principal): a second `hello` with a live guid ADDS a route, a disconnect
  removes only its own; a call to `<guid>|x` goes to every live route and settles
  with the FIRST reply (open TODO: the dropped replies can hide tab desync);
  `peers` lists one row per live route. An `emit` on `<guid>|x` reaches EVERY live
  route too (egress-gated, verb `emit`), and a `web_ws` reply to one tab is mirrored to
  the others as `reply_mirror` (open TODO: tab desync is envisioned, unsolved).
  The root reflect ALSO carries `guid` — the kernel's stable INSTANCE identity,
  minted ONCE at boot (the loader's `_boot`, root only; auto-fills an absent field
  so an explicit `guid` wins and a minted one is never regenerated) and persisted
  into the root record `.fantastic/agent.yaml`. Contributed by the same
  `reflect_root_extra(agent)` hook, alongside `persistence`. Unlike the run-scoped
  `env`/`version` (read inline each reflect), the guid is PERSISTENCE-scoped — it
  round-trips a reboot verbatim, sourced from the persisted meta. A relay peer
  advertises this value as its `owner_guid` (the kernel-INSTANCE guid — distinct
  from a relay LEG's own `guid` on a `relay_connector` record).
- **`render_html`** — duck-typed presentation. Any agent returning
  `{html:str}` from `render_html` can be rendered by a view. This is now
  a FRONTEND pattern (a `*.ts` content agent holds the body in its
  record); the host web serves no render route. Bodies reach the kernel via
  the frontend's own typed WS bridge (`ts/`) — no transport script injected.
- **`get_webapp`** — duck-typed UI discovery. The TS canvas compositor
  iframes any agent that answers `get_webapp` with `{url,
  default_width, default_height, title}`.
- **`reload_html`** — universal page reload. The TS frontend kernel
  subscribes to it; any agent that emits `{type:"reload_html"}` on its
  own inbox triggers a reload of the connected view. `set_html` and the
  canvas frame ⟳ button both go through it.
- **Sidecars persist through the loader** — a bundle with state of its own
  (the `scheduler`'s schedules) sends `persist_blob`/`load_blob` to the root;
  no store ⇒ the verb fails loud (no RAM stand-in).
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
  needs a target. An agent granted `cancel` on the kernel can kill ANY task
  but the kernel's own service tasks (below). There is no per-agent
  "A may watch and kill B's work" edge — that granularity is traded,
  knowingly, for one unambiguous address:
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
    out of `send`. A KERNEL SERVICE TASK — a keepalive loop the root owns (the
    execution flusher, the record persister; `Kernel.is_service_task`) — is
    refused, `{reason:"bad_arg", arg:"task_id"}`, "cancel: task <id> (<verb>) is
    a kernel service task — not cancellable", stays listed in `ps`, and a group
    kill leaves it (and its own work) out.
  Read with `docs of=kernel` (answered by the root alone). Bundles that fork work
  OUTLIVING a dispatch (an `exec_bridge` job) register it with
  `kernel.ctx.spawn_task(...)`, so `ps` sees it and `cancel` reaches it.
- **A backend KIND's record schema** (`ai_core/schema.py`): each AI backend
  module publishes `RECORD_SCHEMA` — its own fields, then the ones every ai_core
  agent takes (`memory_store_id` required; `context_window`, `output_reserve`,
  `context_strategy`, `recent_n` defaulted; `system_prompt` optional — set, it
  REPLACES the default system block on every turn of that agent, main, nudge and
  reply-woken alike, read live; the per-call `send` argument beats it for that
  delivery; a non-string or blank value is refused at the write, `null` clears
  it; `file_bridge_id` + `system_prompt_file` optional — the file_bridge this agent
  reads files THROUGH and a path on it, read as the agent itself (its `can_call`
  names the bridge with `read_stream`/`watch_path`/`unwatch_path`/`watch`, the
  bridge's `callable_by` names it), held under the bridge's `watch_path` so an edit
  is the block from the next turn on, no restart (`ai_core/prompt_file.py`). NOT SET
  (or `file_bridge_id` alone) → nothing is looked up, the default block. SET AND
  UNUSABLE — no `file_bridge_id`, a refused or failed read, an empty file — → the
  turn FAILS loud (`reason: "system_prompt_file"`, the model never called; reflect
  carries `system_prompt_file_error`), never another block.
  PRECEDENCE: per-call > record `system_prompt` > the file >
  the default block — never appended. PLACEHOLDERS in whichever prompt is in force,
  filled per agent and per turn: `{tree}` (the kernel primer), `{self}` (identity
  and grants), `{store}` (the memory store id), `{howto}` (the how-to blocks);
  `{tree}\n\n{self}\n\n{howto}` IS the default block, an unknown `{…}` stays) — built from the constants the
  provider runs on, served in `reflect bundles=all`, and enforced by the same
  module's `validate_meta`: a required field missing (or an enum value outside
  its set) is refused AT THE WRITE, every missing field named with its hint. One
  truth for the set-up page, the write and the turn.
- **Streaming parts** (`ai_core/stream_parts.py`): the model's output is split as it is written into
  parts — `text`, `thinking`, `call` (args streaming) — each streamed as `part_start
  {stream_id, source, kind, name?, target?, to?}` → `part_delta {text}` (batched 50 ms) →
  `part_end` | `part_abort {reason}`. A call part goes to its addressee's inbox (each of
  `to`) as an emit, once `<target_id>`/`<to>` has closed; everything else goes on the
  agent's own inbox tagged with the caller's `client_id`. Deltas are QUIET — inboxes
  only, never the state stream (`emit(..., quiet=True)`); start/end/abort are published.
  ANIMATION, never action: nothing dispatches from a part. A call with no `</call>` is
  NOT a call (`extract_tool_calls` skips it), and ONLY A SUCCESSFUL GENERATION
  DISPATCHES: a pass cut with a block open sends nothing, its closed calls included.
  A dispatched message carries its `stream_id`; a consumer settles a call draft with
  no such message 60 s after its end as FAILED. `rows {after?, limit?}` is a view of
  the agent's in-memory conversation (no store read). A receiver stops a
  generation with the agent's own `stop {stream_id?}` verb (the provider stream closes,
  open parts abort). A CUT pass leaves a marker row on top of the context, never a half
  answer: `[stopped by <receiver>]` or `[model stopped generating]`, then the visible text
  (thinking stripped; `… while thinking` when cut inside a thought), then `[a <tool> call
  to <target> was not sent]` for every call of the pass.
- **AI chains end only by the model's call** (`ai_core`): a turn that emits no
  `<call>` is reminded to act (`[no call was dispatched]`) every time; a DEAD turn
  (only thinking, nothing left once `<think>` is stripped) is discarded — its incoming
  row kept, its empty answer never stored — and rerolled with a `[no answer]` line;
  `done` ends the chain, `await` parks it on what is in flight. A call written WRONG —
  markup carrying the call vocabulary outside every `<call` envelope
  (`<tool_call><function=call name="send">`, `<send>`, a JSON call) — is reported like
  a compiler error: line, caret, and the form to write, built from what the model
  wrote (`ai_core/call_check.py`); the answer's good calls still run, and the report
  says so. A turn whose only output is broken calls ran nothing and counts as silent. Text
  this layer writes to its OWN agent — the no-call reminder, a malformed-call report batch
  (no call outcome in it), an await refusal, an undelivered inquiry answer — is stored as
  `kind: "notice"` (rows, row events), so a view can hide it; the model still reads it as a
  user turn (Qwen's template takes no system message past the first). Only a send to self
  can mark it — a peer's `notice` is ignored. One bound:
  `MAX_SILENT_TURNS` (10) silent turns in a row and the chain GIVES UP loudly
  (`status done reason=gave_up`, `detail.error` = "The LLM was not able to answer …
  Reiterate the inquiry.", also delivered to a `send_to`). Every ai_core tunable
  lives in `ai_core/constants.py`, each with the note that says why.
- **`on_cancel` bundle hook** — the SIGTERM half of `cancel`: cancelling an
  `asyncio.Task` frees NO real resource, and it cannot TELL anyone the work
  ended. Before aborting a task, the substrate awaits the owning agent's hook —
  if the `handler_module` exposes `async def on_cancel(id, task_id, kernel)`, it
  runs (same discovery + optionality as `on_delete`). It runs AS THE AGENT ITSELF
  — its own id is the sender — on every path that kills the task (the `cancel`
  verb, an item's own deadline, `shutdown_agents`), and so does `on_delete`:
  an emit on its own inbox is ungated, every other
  send is gated as that agent. Two things live there:
  - releasing a REAL resource — `exec_bridge.on_cancel` SIGKILLs the job's
    child process, so an aborted task cannot orphan it.
  - closing a message out for a CLIENT — the AI backends (ollama / responses /
    vllm) all re-export `ai_core`'s shared `on_cancel`: it clears the
    in-flight generation (else `status`/`reflect` report a phantom reply forever)
    and emits `status(phase='done', reason='cancelled')` + `done`. The abort
    itself already drops the provider's HTTP stream and the FIFO lock.
  There is NO bundle-private `interrupt` verb anywhere: an in-flight AI `send`
  IS a kernel task, cancelled with `ps` → `cancel` like any other dispatch. (The
  `interrupt` on `exec_bridge` is a DIFFERENT thing — SIGINT to one of ITS
  jobs by `job_id`, the signal-precise twin of `stop`'s SIGKILL; not a call
  cancellation.) Pure-async bundles need nothing.
- **The CALL GATE — minimal, two-sided, explicit grants only**. Every `Agent.send` passes a gate in `kernel/_authz.py`
  (`authorize_call` → the one `Decision`; io_bridge imports that type UP — the
  kernel owns it). The trust boundary is WHO ORIGINATED the send (`_current_sender`).
  **Identity comes ONLY from dispatch**: the kernel sets it when it
  runs an agent's code — its handler, or its `on_cancel`/`on_delete` hook — and the
  sends that code makes (and the tasks it spawns) are that agent's. There is no switch
  to send as anyone else.
  - **Operator send** — `sender is None`: a send made outside every dispatch (the
    CLI, the cold bootstrap, a test driver, a transport handing an inbound frame to
    its leg). The origin root of trust → **ungated**.
  - **Agent send** — passes iff the SENDER's `can_call` grants `(target, verb)`
    (**egress**) AND the TARGET's `callable_by` grants `(sender, verb)` (**ingress**).
    A denial names the `side` (`egress`|`ingress`), egress first.
  **Two record fields only**, `can_call` and `callable_by`, each
  `{key: "*" | ["<verb>", …]}`. A key is a literal agent id, `$self`, `$parent`,
  `$subtree` (every agent BELOW the holder, any depth); `can_call` also takes `<guid>|<agent>` or `<guid>|*`
  (an OUTBOUND peer address — a caller is always local, so such a key in
  `callable_by` could never match and is refused). Tokens resolve relative to the
  grant's HOLDER. Any other key, or a value that is not `"*"` or a list of strings, is
  refused at the write, `invalid_meta`: "callable_by['<key>'] is not a grant key —
  legal keys: an agent id, $self, $parent, $subtree" / "can_call['<key>'] is not a
  grant key — legal keys: an agent id, $self, $parent, $subtree, <guid>|<agent> or
  <guid>|*" / "<field>['<key>'] must be "*" or a list of verb strings". A denial's
  hint: "grants come from above — an ancestor (update_agent on an agent in its
  subtree) or the operator writes can_call and callable_by; an agent never raises its
  own". **`"*"` covers EVERY verb** — there is no
  verb list the gate consults, and NO verb is exempt by name: `docs`, `reflect`, `watch`, `emit`, `state_subscribe` are verbs like any other —
  a caller reads docs with a `docs` grant on the root.
  **Baseline:** `{"$self": "*"}` on BOTH fields, stamped + merged on every agent —
  nothing else. Every other edge is an explicit grant.
  **Kernel verbs answered by the root** (`create_agent`, `ps`, `cancel`, `ledger`,
  `list_agents`, …) take the SAME two-sided check against the root — the root's
  `callable_by` must accept the caller.
  **The tree verbs** (`create_agent`, `update_agent`, `delete_agent`) take that check
  and NOTHING MORE (grants are the only rule): whoever holds the
  verb on the root, both ends, acts on ANY agent it names, its own record included.
  An agent without `update_agent` on the root cannot raise its own authority. A
  missing or non-string operand gets the verb's own `missing_arg` refusal.
  **A join** of an execution re-gates the joiner on the ORIGINAL verb; **a reflect**
  that passes the gate returns the tree whole (roster and `detail:"full"`).
  **Kernel bookkeeping is a direct call, never a gated send** (`Kernel.bookkeeping`
  → `Agent._kernel_run`: the agent's handler runs as itself, no origin, no gate, no
  ledger row): `create_agent` booting its child and putting `agent_created` on the
  parent's inbox, `kernel_start`'s boots, the `shutdown` of a stop, the kernel's
  ledger reads/writes in its memory store, the loader's writes to its own store,
  `static_loader` hydrating its roster, a bundle persisting its sidecar through the
  loader's `persist_blob`/`load_blob` functions.
  **Wiring pairs with granting on BOTH ends** — an operator who wires an egress (a
  scheduler `target` — a fire runs as the scheduler; an AI's `cli`/peer listener, an AI's `memory_store_id`)
  grants that edge in the caller's `can_call` AND opens the callee's
  `callable_by`. An INBOUND relay (`web_ws`/`web_rest`/`ws_bridge`/`relay_connector`)
  composes the gates: `ingress_rule` says WHO may call in, its own `can_call` says
  WHERE those calls land, and the local target's `callable_by` must accept the relay.
  The transport never sends onward itself: `web_ws`/`web_rest` hand each admitted
  frame to the leg (`inbound_call`/`inbound_emit`/`inbound_watch`/
  `inbound_state_subscribe`, as the operator) and the leg performs it as itself; a
  wire bridge's read loop is a task its `boot` spawned, so it is the leg already.
  The `web` host's file route hands each fetch to the host (`fetch_file`), which
  reads as `web`.
- **`on_delete` cascade hook** — substrate calls `await
  agent.on_delete()` depth-first during cascade-delete BEFORE
  detaching the record from `kernel.agents` and the parent's
  `_children`. It tears down PROCESS state only: if the agent's
  `handler_module` exposes `async def on_delete(agent)`, invoke it.
  DISK cleanup is NOT here — the `removed` state event drives a loader
  to rmtree the dir. Bundles port teardown into this function —
  `exec_bridge.on_delete` SIGKILLs its jobs (pipe or PTY), `web.on_delete`
  drains uvicorn, `ws_bridge.on_delete` cancels the read loop.
- **`ephemeral` class flag** — `class Cli(Agent): ephemeral = True`
  means a loader skips it entirely (never written to disk; `save()`
  omits it). Composition is per-process; reboots compose afresh based
  on mode. Use for stateless renderers / debuggers / dispatchers.
- **State-event `sender` + `summary`** — every `send`/`emit` state
  event carries `sender` (the dispatching agent's id, set via a
  task-local contextvar around handler dispatch; external traffic on the
  host's WS/REST legs is sent by the leg itself, so it carries the leg's id) and
  `summary` (a JSON-stringified, bytes-stripped, max-160-char view
  of the payload). A frontend telemetry view uses both to draw
  sender→receiver wires + a last-N message log. The kernel's own LEDGER
  FLUSH publishes nothing on the state stream — its `append` send and every
  send, emit and state event inside its execution (`_state_quiet`: a long-poll
  waiter woken by each flush would feed the next one forever). Its rows stay recorded by the flush row; an id-block
  reservation minted inside it still publishes the root's `updated`, so the
  loader persists the bound.
- **The ROW state event** — every EXECUTION row the kernel puts
  (`rows_put`: `call`, `settle`, `task`, `ack`) is announced on the state stream as
  `{agent_id, kind:"row", row:{id, kind, parent?, caller?, settled?}}` — content-free
  (never a payload or an outcome), so a reader fetches rows only when told and
  polls nothing; gated like every state event (`state_subscribe` on the root), and
  never published from inside the kernel's own ledger flush, so an idle kernel
  publishes nothing (`kernel/_kernel.py` `row_event`). An AI agent's DIALOG rows are
  announced by the agent itself to its `watch`ers, not here.
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
agent gets with one reflect (`fantastic reflect` one-shot, or over WS) —
it is served from the kernel_state bundle, never copied to disk:

Open `ws://host/<any-agent>/ws` and send
`{"type":"call","target":"kernel","payload":{"type":"reflect","readme":true,"bundles":"all"},"id":"1"}`.
The reply carries:

- `readme` — the root readme: every transport (in_process / in_prompt /
  cli / ws / rest / binary-frame), the reflect surface, the reserved
  `kernel` address (the root, whatever its id), the two-kernel (host +
  frontend) model, and the `.fantastic/lock.yaml` daemon rule. Transport/wire
  docs live there, not in the reflect JSON.
- `tree` — the live agent tree (default `all`; `tree:"ids"` for a cheap
  id index).
- `bundles` — with `bundles:"all"`, every installable bundle (what you
  can `create_agent` from), each AI backend with its record `schema` (every
  field: required/type/enum/default/hint; a key is a `credentials` field,
  `{store, ref}`) — the kind's contract
  before any agent of it exists; `bundles:"ids"` for names only.

`docs` (send `kernel` `{type:"docs", of:<id>}`) carries `verbs: {name: signature}`
so an LLM caller can compose any `payload` from the docstring without source
diving. "If you
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
- **ONE KERNEL PER PROCESS.** Bundles keep process state in module-level dicts
  keyed by agent id (`exec_bridge._jobs`, `ai_core._locks`/`_queue`,
  `scheduler._tick_tasks`, `io_bridge._bridges`, `web._servers`,
  `file_bridge._UNDO`), so two kernels in one process holding the same id would
  share it. Resolvable (key per kernel as well as per id), not needed now —
  `main._build_kernel` builds exactly one.
- Agent IDs: `{bundle}_{hex6}` (e.g. `ollama_backend_b04b35`).
  Singletons use the bundle name (`kernel_state`, `cli`).
- Every bundle's `tools.py` defines:
  - per-verb `async def _<name>(id, payload, kernel)` with a
    docstring whose first sentence is the signature (`args: …`) — served by
    the kernel's `docs` verb (oneliners: the signature; detailed: all of it).
  - optional module-level `EMITS` / `CANCEL` / `NOTES` — the rest of the
    detailed docs.
  - `VERBS = {"<name>": _<name>, ...}` dispatch table.
  - `async def handler(id, payload, kernel)` 4-line dispatcher.
- **The OS boundary is `_sys`** — every OS-specific call lives in a package's
  `_sys/` (`posix.py` / `nt.py`, picked once on `os.name`): `kernel/_sys` (the lock,
  the stop signals — SIGINT/SIGTERM/SIGHUP, on Windows SIGBREAK, the CTRL_BREAK a
  polite stop sends, in SIGHUP's place — UTF-8 stdio, the POSIX-shaped `OSError`
  text: `[Errno N] <C runtime strerror>` on every OS), `file_bridge/_sys` (the
  no-follow create, the PRIVATE-file judgment — on Windows the handle's owner +
  a protected DACL naming only this user — which wire paths are absolute: a drive
  or UNC prefix too on Windows), `exec_bridge/_sys` (a job's container — a session,
  a Job Object on Windows — its interrupt, the control socket pair, the PTY —
  ConPTY on Windows — the PATH grammar — PATHEXT on Windows). Call sites never
  branch on the OS; a test of a concept one OS lacks is `_testkit.posix_only`.
- Records in `.fantastic/agents/<id>/agent.yaml` carry only metadata.
  Process-memory state (PTY child, uvicorn server, in-flight tasks)
  is per-process and visible only to the kernel that owns it —
  reflect from the live `serve` to see it, NOT via a fresh
  `fantastic <id> <verb>` (which spawns a separate kernel).

## Storage policy

- Project code lives in version-controlled files in this repo or
  user dirs. **`.fantastic/` is runtime state only** — agent.yaml
  records (the root's carries `guid` and the id counter's `next_id`), per-agent
  sidecars (schedules.yaml, schedule_events.yaml), `lock.yaml` — no readme or
  `docs/` copies (reflect serves them from the bundle). The
  kernel writes NO message log: every message lives in a MEMORY STORE agent
  (`memory/ram_store`, or the durable `memory/sqlite_store`; rows keyed
  `<guid>|<agent id>`). The root's
  `memory_store_id` is mandatory for `kernel_start` (fail loud — "create a
  ram_store and name it in memory_store_id"); execution rows (`call` WITH its
  payload, `settle` WITH its outcome, `task`, `ack`, `flush`) are flushed there
  from RAM, raw bytes reduced to `{len}`; each AI agent names its own
  `memory_store_id` for its conversation, reply bodies never stored (only their
  one-line `origin`). The execution ledger is a projection over the store + RAM.
  All kernel-written files are YAML (block style; format rules in
  `kernel_state/yaml_io.py`); old `.json`/`.xml` files are ignored. Wipe-and-rebuild safe.
- Use a `file_bridge` agent (rooted INSIDE the running dir — the clamp;
  created OPEN with `ingress_rule=allow_all_ingress`) for HTTP-served content:
  `<img src="/<file_id>/file/imgs/foo.png">` works in any html view.

## Disk surface — ALL IO through the gated bridge

**The model (the coherent end-state): ALL IO goes through a `file_bridge` AGENT, no
exceptions, deny-all by default, opened on demand by the operator/LLM.** Two layers, both
unbypassable:
1. **The clamp** — `file_bridge.fs` (`io/file_bridge/.../fs.py`) is the SINGLE module that
   opens an arbitrary path; the running-dir law lives there, so nothing can read/write a
   caller-named path outside its root by going around it (the disk code outside it,
   enumerated at the end of this section, never opens one).
2. **The gate** — the `file_bridge` AGENT wraps `fs` with the io-bridge ingress rule:
   **SEALED / deny-all by default** (every verb except `reflect` denies). A bundle that
   needs disk does NOT import `fs`; it `send`s the text or stream verbs to a file_bridge
   agent by id, or persists a sidecar with `persist_blob`/`load_blob` through the loader
   (the `scheduler`). The operator/LLM **wires + opens** that bridge on demand
   (`create_agent … handler_module=file_bridge.tools allow=… ingress_rule=…`) and grants
   the edge. So the *access control* is real for in-process bundles too — not just the
   clamp; writes fail loud when no surface is wired (no silent RAM).

Code that imports `fs` directly: **the file_bridge agent itself** (it IS the gate);
**`kernel_state`** — its cold boot-read of `.fantastic` (`read_tree`, the chicken-egg root
hydration) AND its `load_tree`/`persist_record`/`forget_record` verbs (a session loader
serving a sub-namespace); **`static_loader`** — its manifest read; **`exec_bridge`**
(`external=True`) for a PTY job's pasted-image scratch dir OUTSIDE the running dir
(/tmp); and **`credential_store`** (`external=True`) — its `file` backend, an ABSOLUTE
path OUTSIDE the running dir by design (`<home>/user/credentials/<id>.yaml`, `<home>` being
`FANTASTIC_HOME`, else `~/.fantastic`; written
0600 via `fs.write_text(..., mode=0o600)`, read via `fs.read_private` — one
`O_NOFOLLOW` open judged by `fstat` on that descriptor), the one bundle that keeps
secrets on disk. **`sqlite_store`** clamps its `path` through `fs.resolve` (the running-dir
law, symlinks included) and hands that path to `sqlite3`, which opens it. The substrate's fixed-path reads (`kernel/_lock.py`, `_env`) and its home guard
(`kernel/_home.py`: a `.fantastic` that IS the fantastic home is refused at the
bootstrap, as in rust and swift — no filesystem access) use plain `pathlib` — the substrate is decoupled from bundles and
cannot import one. (`web` is clean: `/` is a 404 (it renders nothing), `/file/` proxies to
gated agents, own assets are importlib package resources — no `fs`.)

`fs`'s two surfaces, picked STATICALLY per call site (never tried in sequence — that would
be a fallback, which this kernel refuses):
- **clamped** (default) — `fs.read_text(fs.Root(workdir, root), path)`: `root` clamped to
  the kernel's running dir (`Kernel.workdir` — the dir holding its root's `.fantastic`, the
  ONE source; `fs` never reads the process cwd), `path` clamped within `root`. The
  running-dir law guarantees the kernel can't escape its own dir.
- **external** (`fs.read_text(root, path, external=True)`) — `root` is an operator/peer
  base that may live ANYWHERE; `path` still clamped within it. For surfaces that operate
  OUTSIDE the running dir BY DESIGN: an `exec_bridge` PTY job's clipboard-image temp
  scratch dir, and a `credential_store`'s `file` backend (an absolute path, 0600).

The ONLY disk code outside `fs`: the substrate's fixed-path bootstrap (`kernel/_lock.py`,
`_env` — the substrate is decoupled from bundles, cannot import one);
`importlib.resources` reads of a bundle's OWN baked-in assets (readme + `docs/` topic
tree via `Agent._read_readme`, help/index/favicon — a package lookup, not an arbitrary path);
`exec_bridge`'s process plumbing — its `_sys` lists `/proc` (Linux) for a job's session
members and `proc` probes the child's `PATH` dirs for a bare command (`os.path.isfile` +
`os.access`, metadata only), and `paste_image` creates its scratch dir with
`tempfile.mkdtemp` (the image is then written through `fs`, `external=True`); and the
`file_bridge` agent's `list`, which stats the entries `fs.list_dir` returned.

## Path conventions

- All paths relative when invoked via `fantastic <id> <verb>` (cwd = project
  dir).
- The file_bridge's path-safety refuses anything escaping its `root`, and
  the running-dir law clamps the `root` itself inside the kernel's running dir.
- `fantastic` writes `.fantastic/lock.yaml` with `{pid}` and holds an
  flock on it (on Windows a `LockFileEx` lock on byte 2^62 of it — the one rust
  takes); a second serve in the same dir refuses with a clear error
  and a lock no live process holds is taken over. The one-shots (`<id>
  <verb>`, `init`) take the SAME flock — they never judge the pid.
