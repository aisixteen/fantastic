> The verbs the KERNEL answers — signatures, semantics, and who the target is.

# Kernel verbs

These are baked into the substrate, not into any bundle, and they appear in no
agent's `verbs` map. Every reflect ADVERTISES them (as `kernel_verbs`, so you
learn they exist wherever you happen to be looking); only the ROOT ANSWERS them.
Address `kernel`.

They act on the TREE, or on the kernel-wide task table. **The agent you name is
the OPERAND, not the actor** — `parent_id`, `id`, `task_id` say what is acted on.
Sending one to that agent instead gets you a `not_here` refusal naming the fix.

Until 2026-08-24 every agent answered them and the ADDRESSED agent became the
parent of a `create_agent`. That made the target ambiguous — "send it to X" could
mean X performs it, X is created under something, or X is the parent, and only
the last was true — while `parent_id` in the payload was stripped and ignored. A
model asked "But how do I invoke create_agent? Which target?", never resolved it,
and finished sixteen turns having created nothing.

## Composition

`create_agent` — args: `handler_module:str`, `parent_id:str` **(required)**, `id:str?`, `**meta`
: Create a child of `parent_id` and boot it. Name **your own id** to grow your own
  subtree — that needs no grant, because forking what you own is baseline
  authority. Name `kernel_state` for a top-level agent. There is no default
  parent: placement is always stated, never inferred from who sent the call.
  `id` is minted as `{bundle}_{hex6}` when omitted. Everything else becomes the
  record's meta — that is how a bundle is configured (`file_bridge_id`, `mode`,
  `root`, `ingress_rule`, …). Returns the new record.

`update_agent` — args: `id:str`, `**meta`
: Patch a record's meta. **Every field you name is REPLACED, not merged** (a field
  you do not name is untouched), so a dict field must be sent WHOLE —
  `can_call`/`callable_by` included: restate every key you want to keep, the
  baseline `$self`/`$parent`/`$children` among them, or the keys you omit are
  gone. Granting one edge reads like a PATCH and behaves like a PUT.

`delete_agent` — args: `id:str`
: **Cascades** — deletes that agent and its whole subtree, deepest first. Refuses
  with `{error, reason:'locked', id}` if any descendant carries `delete_lock`;
  clear it with `update_agent` first.

`list_agents` — no args
: Every agent's record, flat. Records only — metadata, no descriptions. To learn
  what an agent is FOR, reflect it (or read the roster on a distilled reflect).
  Paged with `limit`/`after`.

## Work in flight

The task table models WORK, Linux-style. A task is ONE in-flight dispatch with id
`"<agent_id>:<token>"` — so a task id already names its own agent, which is why
these verbs need no target. A send issued inside a handler is a CHILD of that
handler's task, so the table is a tree.

`ps` — no args
: `{tasks:[{id, agent, parent, verb, started_at}]}`, kernel-wide. One row per
  in-flight dispatch; `parent` chains them into the send tree. Paged.

`cancel` — args: `task_id:str`
: Kill that task AND ITS SUBTREE (children first, like a process group). Returns
  `{cancelled:bool, killed:[task_id]}`. The blocked caller's `send` resolves to
  `{cancelled:true}` — cancellation never raises out of `send`. An unknown or
  already-finished id returns `{cancelled:false, killed:[]}` rather than
  pretending.

  Cancelling frees real resources where a bundle implements `on_cancel`: an
  `exec_bridge` job's child process is SIGKILLed, an AI backend's in-flight
  generation is cleared and closed out with a `done` event.

  Note the granularity: an agent granted `cancel` on the kernel can kill ANY
  task. Before these moved to the root a manifest could grant "A may watch and
  kill B's work" as one edge; that is gone, knowingly.

## Process

`shutdown_kernel` — no args
: Stop the kernel process gracefully. Blast radius depends on where the kernel
  runs: check the root reflect's `env` — under `env:"container"` this stops (and
  with `--rm` removes) a whole container, not just a process.

## Refusals

Every refused call answers in one shape: `{error, reason, hint?, see?, …}`.
`error` is the human line, `reason` is the code you branch on — `unauthorized`
(carries `side`), `unknown_agent` (carries `agents`), `missing_arg` /`bad_arg`
(carries `arg`), `invalid_meta`, `locked`, `not_here`, `exists`, `unhandled`.
