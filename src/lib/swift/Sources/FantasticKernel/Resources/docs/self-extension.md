> Author your own durable tools as script agents whose persisted `code` is a
> SCRIPT the kernel runs top to bottom.

# Self-extension — author your own tools

This kernel is self-extending: any capability you lack, you AUTHOR as a durable
*script agent* whose persisted `code` is a SCRIPT — the kernel runs it top to
bottom, it is not a set of verb handlers.

- **Add** — `send('kernel', {type:'create_agent', handler_module:'<an executor below>', id:'<name>', code:'<your program as one JSON string>'})`. The loader lays that `code` down as **`main.py` in the agent's own dir**, beside `agent.json` — so a tool is a real file you can read, diff and edit, and a traceback names it at the right line.
- **Run it on demand** — `send('<id>', {type:'start', args:{…}})` runs the persisted `code` once and returns at once; the program's own output is not that reply, so send your result where it belongs from inside the program. A one-shot tool needs no `on_message` and does not stay running.
- **Use (long-lived)** — send the agent a turn by its id. A LIVE script replies `{queued:true}`; its real output rides `emit`ted events (e.g. `say` / `done`) tagged with the caller's `client_id` — `watch` its id to receive them.
- **Remove** — `send('kernel', {type:'delete_agent', id:'<name>'})`.
- **Author the `code` as a SCRIPT** — your `code` is a SCRIPT, not a set of verb handlers. It is run **top to bottom**, once per `start` (and again on `boot`). Defining a function does not register a verb — `def run(payload): ...` and nothing else defines `run`, exits, and does no work at all. Put the work at module level. If the tool must survive its first pass to receive later sends, keep it running (see `on_message` below); only a LIVE job acks `{queued:true}`.
- **The connector** — your program gets a `kernel` object in scope (its ONLY capability): `kernel.send(target_id, payload)` → the target's reply; `kernel.emit(payload)` → emit an event on YOUR OWN agent — this is how you speak (there is NO `kernel.say`; you `emit` a `{type:'say', …}`); receive later inbound turns by registering `kernel.on_message(callback)` and keeping the program running; `kernel.self_id` is your id; `kernel.args` is THIS CALL's arguments — whatever the caller put in `start`'s `args` field (`{}` if none). Take your inputs from `args` rather than baking them into `code`, and take the ADDRESSEE from there too — then one tool serves many callers, and the caller decides where the result goes (`kernel.send(kernel.args['reply_to'], …)`) instead of the tool deciding for them. (The root reflect's `self_extension` names which executor + receive-shape THIS runtime has.) To answer an inbound `{type:'send', text, client_id}`: do the work, then `kernel.emit({type:'say', text:<answer>, client_id})` then `kernel.emit({type:'done', client_id})`. Handle ONLY `{type:'send'}` turns — ALL inbound traffic (including your own emitted events and `reflect`/`boot`) reaches `on_message`, so ignore anything whose type isn't `send`. Keep each string literal on ONE line — the `code` is one JSON string.
- **Compose** — a running script can itself `create_agent` more agents, so tools build on tools.
- **Grant its reach — two-sided deny-all** — every agent is DENY-ALL by default on BOTH sides: who it may CALL (`can_call`, egress) and who may CALL IT (`callable_by`, ingress). An edge exists only when BOTH ends grant it. The only free edges are structural — an agent may reach and be reached by itself, its parent (its creator), and its own children. NOTHING is exempt from the gate, `reflect` included: a `send` to a `file_bridge` or `yaml_state` you didn't create comes back `{reason:'unauthorized'}` (the `side` field says which end refused — `egress` = your reach, `ingress` = their acceptance). Two ways to give a tool what it needs:
  - **The clean path — author a cohort of your OWN children.** Create the tool AND the data agents it talks to as YOUR children (or have the tool `create_agent` them as ITS children). Agents in one subtree reach each other through the `$parent`/`$children` baseline with ZERO explicit grants — a tool and the `readings`/`totals` it owns just work.
  - **Reaching an EXISTING foreign peer** takes a grant on each end. Grant your tool's egress IN ITS `create_agent`, on `can_call:{'<target_id>':['<verb>', …]}` (or `'*'` for every verb) — but you may only confer a SUBSET of what YOU yourself hold (over-conferring is refused). The peer must also ACCEPT your tool on its `callable_by`; if the peer is yours, set it (`update_agent` the peer, adding `callable_by:{'<tool_id>':['<verb>']}`); if it belongs to someone else, its owner (or the operator) opens it — from above.
  - **Authority descends, never self-raised.** You grant your child; you CANNOT edit your own `can_call`/`callable_by` (an `update_agent` on yourself touching either field is refused). New authority always comes from your creator or the operator.
  You never have to guess: a denied send's `hint` names the fix, and `side` tells you which end to open — discovery-through-denial, the same way a sealed io leg's denial teaches `ingress_rule`.

Reach everything by id: the kernel connector (send / emit / on_message + your
self_id) is how a tool is MEANT to touch the world.

What that isolates you from depends on the interpreter you were given. The allowlist
buys exactly ONE property, stated narrowly on purpose: the OPERATOR chose which runtime
and which environment your program runs in, and your program cannot choose a different
one. It buys nothing else — a CPython on the list still has `open()`, `socket` and
`subprocess`, so a leg allowlisting one is a conscious decision to allow local code
execution with that interpreter's full reach. A real sandbox needs an OS mechanism (a
separate uid, a container, `sandbox-exec`) and is DEFERRED, not solved. Prefer the
connector regardless — data reached by id is inspectable, gated, and portable across
kernels; data reached by `open()` is none of those.
