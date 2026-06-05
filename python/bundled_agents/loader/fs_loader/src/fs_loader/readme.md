# This is a Fantastic kernel.

A tree of agents. One primitive: `send(target_id, payload) -> reply`.
Every agent answers `{"type":"reflect"}` — identity, verbs, state.

## One-call bootstrap

`reflect readme=true` — your first move: the addressed agent's identity
plus this readme, in one call. Read-only and lock-free. The default
target is the tree root (alias `kernel`), so `reflect readme=true` alone
hands you the whole substrate description (this file) next to the live
tree. Start there.

## The reflect surface

`reflect` is the one discovery verb. It returns the ADDRESSED agent
uniformly — `{id, sentence, display_name, description?, verbs?, ...state}`
— the root is NOT special. Compose what you get back with three flags:

- `tree=all|ids|none` (default `all`)
  - `all`  — the agent's whole subtree, nested + distilled
             (`{id, parent_id, handler_module, display_name, description?, children}`).
  - `ids`  — a flat list of descendant ids. Cheap. Use it to scan.
  - `none` — just the agent, no subtree.
- `bundles=all|ids|none` (default `none`)
  - `all`  — every installable bundle as `{name, handler_module}`.
  - `ids`  — just the bundle names.
  - `none` — omit (the usual case).
- `readme=true|false` (default `false`)
  - `true` — attach the addressed agent's `readme.md` (string or null),
             one agent at a time.

## Context hygiene

Reading reflect output is cheap; readmes are not. To explore without
flooding your context: SCAN with `reflect tree=ids bundles=ids` (ids
only), then DRILL into ONE id at a time with `reflect tree=all` or
`reflect readme=true`. Never pull every agent's readme at once — fetch
the single readme for the agent you are about to act on.

## The `description` field

Any agent may carry a short `description` — a one-line "what this agent
is for", set at create or update:
    create_agent handler_module=<bundle>.tools description="..."
    update_agent id=<agent> description="..."
It surfaces in every reflect (top-level and in each tree node), so a
single `reflect tree=all` tells you what every agent in the tree does at
a glance — no per-agent readme needed just for the overview.

## Memory — durable state, as agents, everywhere

Memory here is just agents. A `yaml_state` agent is a YAML key-value
store you mount ANYWHERE in the tree: under the root for global memory,
under any agent for that agent's own local memory — as many as you want,
long- or short-term, plus component/UI state. The `mode` meta is just
discipline (verbs are identical): `mem` = facts to remember (names,
preferences, decisions); `data` = current state (UI, hyperparams,
selection). For an LLM agent, the memory agents mounted under it are
auto-loaded into its context on boot — you do not fetch memory, it is
already present; you only `set` what's worth keeping, under a descriptive
namespaced key (`user.name`, `decision.db`). Mount one with `create_agent
handler_module=yaml_state.tools mode=mem|data`; `reflect readme=true` on
it for the verb guide (read / keys / set / delete / replace /
state_yaml).

## Transports — every way to talk to this kernel

The envelope is always `{"type":"<verb>", ...fields}`. `reflect` is
universal; per-verb signatures come from each agent's reflect `verbs`.

- in_process  — `send(target_id, payload)` from code inside the kernel.
- in_prompt   — LLM tool-call loops emit
                `<send id="<agent_id>" payload='{"type":"<verb>", ...}'/>`.
- cli         — `fantastic <agent_id> <verb> [k=v ...]`; shorthand
                `fantastic reflect [<agent_id>]`. One-shot; refused while
                a daemon owns the dir (use the web surface then).
- ws          — `ws://host:port/<agent_id>/ws`; frames
                `{"type":"call","target":..,"payload":..,"id":..}`.
- rest        — `POST http://host:port/<rest_id>/<target_id>` body=payload;
                browser-pastable `GET .../<rest_id>/_reflect[/<target>][?readme=1]`.
- binary      — for byte-heavy payloads, a WS binary frame
                `[4-byte BE uint32 H | H-byte JSON header | M-byte body]`;
                `_binary_path` names the body field. Skips base64.

`kernel` is an alias for the tree root: send any verb to `kernel` and it
resolves to the root agent — handy for reflecting the whole tree without
knowing the root's id.

## Reach this kernel

Check `.fantastic/lock.json` first — it holds `{pid}`. One kernel per
dir, so the two paths are mutually exclusive:

**A daemon IS running** (lock.json has a live pid). The one-shot CLI is
locked out — go through the daemon's surface. Find it in the tree: a
`web.tools` node carries the HTTP `port`; its children are the surfaces.
  - `web_rest.tools` child → `POST http://localhost:<port>/<rest_id>/<target>`
    body=`{"type":"<verb>",...}`; or browser-pastable
    `GET http://localhost:<port>/<rest_id>/_reflect[/<target>][?readme=1]`
  - `web_ws.tools` child → `ws://localhost:<port>/<agent>/ws`
    (frames: `{"type":"call","target":..,"payload":..,"id":..}`)
  To get the tree itself: `GET http://localhost:<port>/<rest_id>/_reflect`.

**No daemon running** (no live pid). The one-shot CLI works — each call
spawns a fresh kernel, reads disk, dispatches, exits:
  - `fantastic reflect` — the agent tree (add `bundles=all` for the catalog)
  - `fantastic <id> <verb> [k=v ...]` — call any verb on any agent
  (one-shots don't see live process-memory state — they're disk-only.)

**A daemon is running but there's no `web.tools` node** → no HTTP surface
and one-shots are locked out. Tell the user to add one (don't guess a
port yourself):
    fantastic fs_loader create_agent handler_module=web.tools port=<N>
    fantastic <web_id> create_agent handler_module=web_ws.tools
    fantastic <web_id> create_agent handler_module=web_rest.tools
    fantastic <web_id> create_agent handler_module=fs_loader.tools \
        root=.fantastic/web watch=false alias=web_loader      # the frontend store (see "Two kernels")
    fantastic web_loader persist_record \
        record='{"id":"<root_id>","handler_module":"<frontend-root>.ts"}' # seed the frontend
        # compositor ROOT (an opaque `.ts` record) that panels parent to; reflect the TS
        # frontend kernel served from ts/dist over the bridge for its actual root/view kinds
...then they restart the daemon.

## Stopping the kernel

`send kernel {"type":"shutdown_kernel"}` gracefully stops the whole kernel
PROCESS. It acks `{"type":"shutdown_kernel","ok":true}` FIRST, then (after the
reply is on the wire) releases `.fantastic/lock.json`, drains in-flight work,
stops the HTTP/WS listeners, and exits code 0. Reachable over both `web_rest`
(POST) and `web_ws`, so a remote operator can stop the kernel with one verb —
no `kill`, no backend knowledge.

This is **PRIVILEGED and root-only**: it is gated to the kernel's own control
surface (the tree root, alias `kernel`); sending it to any child agent returns
an error. It is **backend-agnostic**: the kernel is PID 1 in a container, so its
exit stops the container (auto-removed if it was run with `--rm`); a bare host
process simply dies. Either way the port goes down and the lock releases — the
caller does not need to know how the kernel was launched. Idempotent / one-shot:
a second call lands on a dead port. The bind-mounted `.fantastic/` workdir
persists across the stop.

## Mental model

Agents are recursive — an agent can own children. `create_agent
handler_module=<bundle>.tools` spawns one (as a child of whatever you
call it on); `delete_agent` cascades depth-first. On disk each agent is
`<dir>/agent.json` with children nested under `<dir>/<children_dir>/<child>/`.
The container name is declared config (the root record's `children_dir`,
default `agents`) — set `host_agents` here / `web_agents` on a `web_loader`
for a self-describing layout.

## Two kernels — host + browser frontend

This is the HOST kernel (Python, `*.tools` bundles). A second kernel — a
browser FRONTEND (`ts/`, `*.ts` bundles) — federates over the SAME WS and
renders all UI. The `handler_module` SUFFIX says where an agent runs:

- `*.tools` → HERE. `create_agent handler_module=<x>.tools` on the host tree.
- `*.ts`    → the FRONTEND. Do NOT `create_agent` these here (the host
  weak-loads `*.ts` → inert). You SPAWN one by `persist_record`-ing its
  record into the frontend store `web_loader`; the browser hydrates + runs it.

There is NO server-side page/render route, no `mount` verb, no per-verb REST
surface — don't invent those. The host serves only STATIC files (via a `file`
agent) and carries `send()`/events over the WS bus. ALL UI is `*.ts` agents the
browser renders. A "web panel" is a frontend view record (`handler_module`
ending in `.ts`) the frontend kernel renders — never a `web.tools` page.

### Recipe: add an interactive panel
`web_loader` is a second `fs_loader` rooted at `.fantastic/web/` (alias
`web_loader`; created once — see "Reach this kernel"). The browser `load_tree`s
it on boot and `persist_record`s changes back. These records are stored OPAQUELY
here (`handler_module` ending in `.ts` → inert on the host); the TS frontend
kernel served from `ts/dist` is what hydrates + renders them. Reflect that
frontend over the bridge to discover its compositor root and view-agent kinds.
The store holds ONE compositor root the frontend renders, seeded once at setup —
`load_tree` and look for it. Panels are its children. (A child of a dir with no
`agent.json` is unreachable, so the compositor root record MUST exist before you
parent to it — that's why setup seeds it.) Then persist a frontend view record
(a `.ts` `handler_module`) parented to that root — e.g.:

    send web_loader {"type":"persist_record","record":{
      "id":"panel1", "handler_module":"<frontend-view>.ts", "parent_id":"<root_id>",
      "html":"<button id=run>Run</button> <pre id=out></pre><script>…</script>"
    }}

The `html` IS the panel BODY (a fragment — frontend state in the record). The
frontend compositor renders it as a sandboxed iframe automatically (no mount),
and INJECTS a `fantastic` connector the body uses to reach the JS kernel — no
import, no URL:

    let job = null;
    fantastic.watch("<python_runtime_id>", (ev) => {          // live progress, host → here
      if (ev.job_id === job && ev.type === "progress") {
        out.textContent = ev.line;                            // updates in place
        fantastic.emit("panel2", {type:"v", value: ev.line}); // → another panel, by id
      }
    });
    run.onclick = async () => {
      const r = await fantastic.send("<python_runtime_id>",   // a HOST python_runtime you create here
        {type:"start", code:"import random;print(random.randint(0,9))"}); // NON-BLOCKING — returns a job_id
      job = r.job_id;                                         // output streams as `progress` events
    };

`python_runtime` is an ASYNC job spawner: `start` runs `python -c <code>` in the
background (many in parallel) and returns `{job_id}` at once; it streams stdout/
stderr as `progress` events + a final `job_done` (with the collected output); use
`status`/`stop` by job_id. There is no blocking "run-and-wait" — watch the events
or poll `status`. (This is the generalized, improved `execute_python`.)

Connector surface (mirrors the kernel): `send(target,payload)→reply`,
`emit(target,payload)` (fire-and-forget), `watch(src,cb)→unwatch`,
`onMessage(cb)` (messages sent to THIS panel's id). A receiver panel:
`fantastic.onMessage(p => out.textContent = p.value)`.

### How it routes (one rule: only the JS kernel)
The connector talks ONLY to the browser JS kernel — never the host directly. The
JS kernel is the SOLE owner of the host link and abstracts local-vs-host, so you
address EVERY agent by id the same way:
- another FRONTEND panel (e.g. `panel2`) → delivered locally, in-browser.
- a HOST agent (e.g. a `python_runtime` you `create_agent` here) → routed over
  the kernel bridge; its reply / emitted events come back the same way.
- a HOST agent PUSHES to a panel the same way: it emits on its own id; the panel
  `watch`es it and receives. Host backends a view fronts are weak PEERS by id —
  closing a view leaves the backend running.
Frontend code NEVER addresses the host directly — always the JS kernel.

## Meta-possibility — any routine orchestrates the whole substrate

Every routine reaches every agent by id through its connector — a host
`python_runtime` job (its spawned code gets a `kernel`: send/emit/reflect/watch/
on_message) and a browser view-agent's JS (its injected `fantastic`) alike. So
from EITHER kernel a routine can: read memory from anywhere (`send(<state_id>,
{type:"read"})`), run an inference turn (`send(<ai_id>, {type:"send",
system_prompt, text})`), and/or spawn a compute job (`send(<py_id>,
{type:"start", code})`) — by id, regardless of which kernel owns the target.
Memory, inference, and compute are interchangeable units you wire from anywhere: a
python routine can call an AI; an AI or a JS panel can spawn a python routine; all
read the same memory. This is why a step written as code and a step written as an
LLM call are substitutable. Bind by id + duck-typed verbs, never by concrete type.
An AI worker's result is not plumbed: the per-call prompt names who listens, the
system prompt carries the `send` signature, and the model routes its own output
(to one addressee or many).

## If you are a terminal here

You may be `claude` in a `terminal_backend` PTY on this host. The view
rendering you (`terminal_view`) lives in the browser frontend, not the
agent tree — reflect yourself and walk `parent_id` up to see what you're
under. To add FRONTEND views beside yourself (any frontend view record —
`handler_module` ending in `.ts`), `persist_record` them to `web_loader` (see
"Recipe" above); to add HOST agents, `create_agent` here. (Lost your own id?
`reflect tree=ids` lists every one — find the `terminal_backend` whose PTY
is yours.)

To learn ANY agent: reflect it with `readme=true`.
