# This is a Fantastic kernel.

A tree of agents. One primitive: `send(target_id, payload) -> reply`.
Every agent answers `{"type":"reflect"}` — identity, verbs, state.

## One-call bootstrap

`reflect readme=true` — your first move: the addressed agent's identity
plus this readme, in one call. Read-only and lock-free. The default
target is the tree root (alias `kernel`), so `reflect readme=true` alone
hands you the whole substrate description (this file) next to the live
tree. Start there.

## First contact — if you arrived via the terminal

If a `fantastic` daemon was launched on a tty (or you are an LLM in a
`terminal_backend` PTY here), it greeted you with a TWO-PHASE intro: first
`[fantastic] … — booting…` (identity + a compact PULL/PUSH control-plane map),
then `[kernel] up` (the live REST/WS attach coordinates, or a compose hint if no
web surface exists yet). That banner is the SHORT map; this readme
(`reflect readme=true`) is the full one it pointed you to.

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
namespaced key (`user.name`, `decision.db`).

**Persistence is wired, not automatic.** The tree lives in RAM until the
root has somewhere to write: it auto-persists records (and every agent's
sidecars) ONLY through a `file_bridge` provider it DISCOVERS — the first
`file_bridge` child of the root whose `root` resolves to `.fantastic`. NO
provider ⇒ no persistence, lost on exit; there is no direct-write
fallback. So wire the store FIRST, before anything worth keeping:

    create_agent handler_module=file_bridge.tools id=store root=.fantastic ingress_rule=allow_all

The `file_bridge` edge is SEALED by default — `ingress_rule=allow_all`
opens it; `reflect` then shows `persistence: {provider: "store"}`. A
`yaml_state` agent owns NO disk of its own: it persists `state.yaml`
THROUGH the file_bridge named by its `file_bridge_id` meta, so mount it
BOUND to the store — `create_agent handler_module=yaml_state.tools
mode=mem|data file_bridge_id=store`. Until that's wired, `set` / `delete`
/ `replace` FAILFAST (`file_bridge_id required`) rather than silently
dropping to RAM. One store serves both the root's records AND every
agent's sidecar: the sidecar lands at `agents/<id>/state.yaml`, next to
that agent's own `agent.json` (store-relative — no `.fantastic/.fantastic/…`
nesting). `reflect readme=true` on the agent for its verb guide (read /
keys / set / delete / replace / state_yaml).

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
- browser-bus — in a browser, `BroadcastChannel("fantastic")` carries
                `{type, target_id, source_id, ...}` between iframes,
                bypassing the kernel entirely (`fantastic_transport().bus`).

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
    fantastic core create_agent handler_module=web.tools port=<N>
    fantastic <web_id> create_agent handler_module=web_ws.tools
    fantastic <web_id> create_agent handler_module=web_rest.tools
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
call it on); `delete_agent` cascades depth-first.

To orient inside the tree: reflect yourself, then walk `parent_id` up to
your ancestors or list children to reach what you own. `reflect
tree=ids` shows every id at once. From any container agent you can spawn
children next to existing ones, inspect them, or cascade one out.

(No UI/view bundles live in this host — the frontend lives in `ts/` and
is served generically over the bridge. Reflect a TS frontend kernel from
`ts/dist` if you need one.)

To learn ANY agent: reflect it with `readme=true`.
