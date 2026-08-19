> The seven verbs EVERY agent answers natively — signatures and semantics.

# System verbs

These are baked into the substrate, not into any bundle. They appear in no
agent's `verbs` map, because every agent answers them: you can `ps` any agent,
`cancel` any dispatch, `create_agent` under any parent. A root reflect lists
their NAMES; this page is where their arguments live.

## Composition

`create_agent` — args: `handler_module:str`, `id:str?`, `**meta`
: Spawn a child agent under the addressed agent and boot it. `id` is minted as
  `{bundle}_{hex6}` when omitted. Everything else becomes the record's meta —
  that is how a bundle is configured (`file_bridge_id`, `mode`, `root`,
  `ingress_rule`, …). Returns the new record.

`update_agent` — args: `id:str`, `**meta`
: Patch a record's meta in place. Use it to wire an agent after the fact (set a
  `file_bridge_id`) or to open a sealed io leg (`ingress_rule=allow_all`).

`delete_agent` — args: `id:str`
: Cascade-delete an agent and its whole subtree, deepest first. Refuses with
  `{error, locked:true, id}` if any descendant carries `delete_lock` — clear it
  with `update_agent` first.

`list_agents` — no args
: Every agent's record, flat. Records only — metadata, no descriptions. To learn
  what an agent is FOR, reflect it (or read the roster on a distilled reflect).

## Work in flight

The task table models WORK, Linux-style. A task is ONE in-flight dispatch with
id `"<agent_id>:<n>"`. A send issued inside a handler is a CHILD of that
handler's task, so the table is a tree.

`ps` — no args
: `{tasks:[{id, agent, parent, verb, started_at}]}`. One row per in-flight
  dispatch; `parent` chains them into the send tree.

`cancel` — args: `task_id:str`
: Kill that task AND ITS SUBTREE (children first, like a process group).
  Returns `{cancelled:bool, killed:[task_id]}`. The blocked caller's `send`
  resolves to `{cancelled:true}` — cancellation never raises out of `send`. An
  unknown or already-finished id returns `{cancelled:false, killed:[]}` rather
  than pretending.

  Cancelling frees real resources where a bundle implements `on_cancel`: a
  `python_runtime` job's child process is SIGKILLed, an AI backend's in-flight
  generation is cleared and closed out with a `done` event.

## Process

`shutdown_kernel` — no args, ROOT ONLY
: Stop the kernel process gracefully. A non-root agent answers with an error.
  Note the blast radius depends on where the kernel runs: check the root
  reflect's `env` — under `env:"container"` this stops (and with `--rm` removes)
  a whole container, not just a process.
