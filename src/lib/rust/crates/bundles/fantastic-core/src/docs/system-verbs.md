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
and finished sixteen messages having created nothing.

## Composition

`create_agent` — args: `handler_module:str`, `parent_id:str` **(required)**, `id:str?`, `**meta`
: Create a child of `parent_id` and boot it. An agent names **its own id** — it
  creates only its OWN children (`side:'mutator'` otherwise, "<you> may only
  create_agent its own children — <parent_id> is not one"), and needs
  `create_agent` granted on both ends (its `can_call` on the root, the root's
  `callable_by` for it). The operator names any parent (`kernel_state` for a
  top-level agent). There is no default
  parent: placement is always stated, never inferred from who sent the call.
  `id` is minted as `{bundle}_{hex6}` when omitted. Everything else becomes the
  record's meta — that is how a bundle is configured (`mode`, `root`, `allow`,
  `ingress_rule`, …). The child's `boot` is the kernel's own bookkeeping, run as
  the child — never a send gated against you. Returns the new record.

`update_agent` — args: `id:str`, `**meta`
: Patch a record's meta. **Every field you name is REPLACED, not merged** (a field
  you do not name is untouched), so a dict field must be sent WHOLE —
  `can_call`/`callable_by` included: restate every key you want to keep, the
  baseline `$self` among them, or the keys you omit are gone. Granting one edge reads like a PATCH and behaves like a PUT.

  **Who may:** an agent updates only its OWN DIRECT CHILDREN — the record `id`
  names must have the sender as its parent — after the two-sided gate on the root
  (`update_agent` in its `can_call` on the root and in the root's `callable_by`).
  Otherwise `{reason:'unauthorized', side:'mutator'}`, "<you> may only
  update_agent its own children — <id> is not one" — an id naming no agent
  included. A missing or non-string `id` (or `parent_id` for `create_agent`) gets
  the verb's own `missing_arg` refusal instead. So no agent edits its own record:
  grants come from above — the agent's parent or the operator writes them.

`delete_agent` — args: `id:str`
: **Cascades** — deletes that agent and its whole subtree, deepest first. Refuses
  with `{error, reason:'locked', id}` if any descendant carries `delete_lock`;
  clear it with `update_agent` first. Gated exactly as `update_agent` is:
  an agent deletes only its OWN DIRECT CHILDREN (`side:'mutator'` otherwise).

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
  generation is cleared and closed out with a `done` event. Each hook runs as its
  own agent, whoever cancelled (as does `on_delete`).

  Note the granularity: an agent granted `cancel` on the kernel can kill ANY
  task but the kernel's own service tasks — the keepalive loops the root owns
  (the execution flusher, the record persister): `cancel` refuses one
  (`bad_arg`), it stays in `ps`, and a group kill leaves it out. Before these
  moved to the root a manifest could grant "A may watch and kill B's work" as
  one edge; that is gone, knowingly.

## Executions

Every call is an EXECUTION: a `call` row when it is dispatched, a `settle` row with
its outcome when it ends. The kernel holds them in RAM and FLUSHES them into its
memory store (the root's `memory_store_id`, under the kernel's own key); a settled,
flushed execution leaves RAM. A call carrying an `id` is NAMED: a second send of the
same id returns the recorded outcome instead of running again — across a flush and a
restart, because the store is read before dispatch.

A call's rows hold its payload and what its settle keeps, in clear — the kernel
masks nothing. A value you must not have recorded does not ride a call: keep it in
a `credential_store` and name the ref. A re-send that JOINS an execution is gated
on the execution's original verb, and receives its recorded outcome.

`ledger` — no args
: `{executions:[{id, target, status, attempts, retries, named, outcome?, acked?}]}` —
  a projection over the store plus what is not yet flushed, scoped to what YOU
  originated. Paged. A `ledger` or `ack_deadletter` execution answers only the
  caller that sent it: naming ANOTHER caller's (its id or name) to join it is
  refused — make your own call under a new id. What a settle KEEPS of an outcome: a NAMED call's whole result
  (its dedup contract), a dead letter's error text, and `{status, bytes}` for
  everything else — a reply body is its caller's, never the kernel's record.

`ack_deadletter` — args: `id:int|str` (an execution id, or its name)
: A `failed`/`timed_out`/`cancelled` execution is a DEAD LETTER, announced by the
  `on_deadletter` event on the root. There is no re-send: inspect it, retry with a
  NEW id if you choose, then acknowledge it. An acknowledged dead letter may be
  pruned; an unacknowledged one is kept. Returns `{acked:true, id}`.

## Process

`kernel_start` — no args
: The start sequence. The root must name `memory_store_id` — without it the start
  refuses (create a `ram_store` and name it there if you have none). Boots the
  root, then the store, starts flushing, then boots every other agent in
  registration order. Idempotent. Returns `{started, store, booted:[ids]}`.

`shutdown_agents` — no args
: Stops the AGENTS, not the process: new work refused, `shutdown` sent to every
  agent advertising it, in-flight work waited for, the rest cancelled (into the dead
  letters). Returns `{drained, shut:[ids], cancelled:[ids]}`.

`kernel_stop` — no args
: Stop the kernel PROCESS gracefully — drain, every call settles, the last rows
  flush, agents shut down, exit. Blast radius depends on where the kernel runs:
  check the root reflect's `env` — under `env:"container"` this stops (and with
  `--rm` removes) a whole container, not just a process.

## Refusals

Every refused call answers in one shape: `{error, reason, hint?, see?, …}`.
`error` is the human line, `reason` is the code you branch on — `unauthorized`
(carries `side`), `unknown_agent` (carries `agents`), `missing_arg` /`bad_arg`
(carries `arg`), `invalid_meta`, `locked`, `not_here`, `exists`, `unhandled`.
