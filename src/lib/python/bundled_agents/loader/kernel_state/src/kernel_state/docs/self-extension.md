> Author durable tools as script agents — the persisted `code` is a SCRIPT run top to bottom.

# Self-extension — author your own tools

This kernel is self-extending: any capability you lack, you AUTHOR as a durable
*script agent* whose persisted `code` is a SCRIPT — the kernel runs it top to
bottom, it is not a set of verb handlers.

- **Add** — `send('kernel', {type:'create_agent', handler_module:'<the executor the root reflect names>', id:'<name>', code:'<your program as one JSON string>', interpreters:['<an allowlisted interpreter>'], shim:'python', ingress_rule:'allow_all'})`. The loader lays that `code` down as **`main.py` in the agent's own dir**, beside `agent.json` — so a tool is a real file you can read, diff and edit. The three fields after `code` are not decoration: without them the agent is created and never runs (next three bullets).
- **Name an interpreter — there is NO default** — the executor spawns ONLY what its own record allowlists in `interpreters`, and it is the single process surface in this kernel, so there is no way around that list. Empty allowlist ⇒ every `start` refuses and says so; exactly one entry ⇒ that one runs; several ⇒ choose with `interpreter:'<one of them>'`. The kernel will not guess a path, so DISCOVER one instead of inventing it: `send('<an executor agent that already exists>', {type:'reflect'})` — its reply advertises that leg's `interpreters` allowlist (and the `shims` it can serve). Copy an entry onto the agent you create. If no executor agent exists yet, ASK the operator for the path; a made-up one is refused, and the refusal names the legal set.
- **Open the gate** — the executor is an io leg and a fresh one is SEALED: every verb except `reflect` denies. `ingress_rule:'allow_all'` is what makes your tool runnable. Omit it and `create_agent` still reports success while the program never runs — the `boot` that would have started it is denied and that reply is discarded, so the failure is silent. If a `start` comes back `{reason:'unauthorized'}`, that is this, and the reply's `hint` is the fix.
- **`shim` is what puts `kernel` in scope** — the executor publishes a WIRE (an inherited control fd, length-prefixed JSON frames) and nothing more. `shim:'python'` prepends the python client, so your program gets the `kernel` object everything below assumes; the executor's reflect lists which shims this build serves. Name no shim and you must speak the wire yourself (`{type:'reflect', readme:true}` on the executor agent for the frame vocabulary).
- **Run it on demand** — `send('<id>', {type:'start', args:{…}})` runs the persisted `code` once and returns `{job_id}` AT ONCE; the program's own output is not that reply, so send your result where it belongs from inside the program. A one-shot tool needs no `on_message` and does not stay running.
- **Use (long-lived)** — send the agent a turn by its id. It replies `{queued:true}`; its real output rides `emit`ted events (e.g. `say` / `done`) tagged with the caller's `client_id` — `watch` its id to receive them.
- **Remove** — `send('kernel', {type:'delete_agent', id:'<name>'})`.
- **Author the `code` as a SCRIPT** — it runs **top to bottom**, once per `start` and again on `boot`. Defining a function does not register a verb: `def run(payload): …` and nothing else defines `run`, exits, and does no work at all. Put the work at module level. If the tool must outlive its first pass to receive later sends, keep it running (see `on_message` below) — only a LIVE job replies `{queued:true}`.
- **The connector** — with a shim named, your program gets a `kernel` object in scope (its ONLY capability): `kernel.send(target_id, payload)` → the target's reply; `kernel.emit(payload)` → emit an event on YOUR OWN agent — this is how you speak (there is NO `kernel.say`; you `emit` a `{type:'say', …}`); receive the next inbound turn with `kernel.on_message()` called in a LOOP (js/lua — js `await`s it; `null`/`nil` means you were deleted), or with the python shim by registering `kernel.on_message(callback)`; `kernel.self_id` is your id; `kernel.args` is THIS CALL's arguments — whatever the caller put in `start`'s `args` field (`{}` if none). Take your inputs from `args` rather than baking them into `code`, and take the ADDRESSEE from there too — then one tool serves many callers, and the caller decides where the result goes (`kernel.send(kernel.args['reply_to'], …)`) instead of the tool deciding for them. (The root reflect's `self_extension` names which executor + receive-shape THIS runtime has.) To answer an inbound `{type:'send', text, client_id}`: do the work, then `kernel.emit({type:'say', text:<answer>, client_id})` then `kernel.emit({type:'done', client_id})`. Handle ONLY `{type:'send'}` turns — ALL inbound traffic (including your own emitted events and `reflect`/`boot`) reaches `on_message`, so ignore anything whose type isn't `send`. Keep each string literal on ONE line — the `code` is one JSON string.
- **Compose** — a running script can itself `create_agent` more agents, so tools build on tools.

Reach everything by id: the kernel connector (send / emit / on_message + your
self_id) is how a tool is MEANT to touch the world.

What that isolates you from depends on the interpreter you were given. The
allowlist buys exactly ONE property, stated narrowly on purpose: the OPERATOR
chose which runtime and which environment your program runs in, and your program
cannot choose a different one. It buys nothing else.
Everything a program can touch, it touches with whatever that interpreter grants —
a CPython on the list still has `open()`, `socket` and `subprocess`, so a leg
allowlisting one is a conscious decision to allow local code execution with that
interpreter's full reach. Where an engine happens to grant less, you get less:
a js engine with no fs bindings, or a lua state with `io`/`os` unloaded, has
nothing else to reach for — but that is a property of the engine, not a promise
this kernel makes. A real sandbox needs an OS mechanism (a separate uid, a
container, `sandbox-exec`) and is DEFERRED, not solved. Prefer the connector
regardless — data reached by id is inspectable, gated, and portable across
kernels; data reached by `open()` is none of those.
