> A durable program is a FILE you write plus a command that runs it. Compose the agents you have, stream a program's bytes to a file, run it through the command executor. There is no `code` field.

# Self-extension

**A durable program is a FILE plus a command — not a `code`-carrying agent.**

A whole program escaped into a JSON string is exactly how a call gets lost to one
stray quote, so a program is never carried that way. The way that works, and makes
something that genuinely persists and re-runs:

1. **Write the program to a file** on the file leg, its bytes carried over a
   stream so nothing is escaped — `write_stream {path, bytes:<raw>, truncate:true}`.
   `bytes` is VERBATIM: write the program between `<bytes>` and `</bytes>` exactly
   as it should land (real newlines, real quotes, nothing escaped) and close the
   call; the program ends at the first `</bytes>`.
2. **Run it** with the command executor: `start {command:'python',
   args:['<your file>'], …}`. The file is durable on disk, so the SAME command
   run again later re-executes the SAME program against whatever the inputs hold
   THEN — which is what "build a thing that re-runs" means.

The program reaches the other agents itself, over its control wire, exactly as
any agent does. Assemble these pieces and you have built something durable
without any `code` field at all.

## What you can do

- **Compose the agents you were given.** Send `kernel` `<type>list_agents</type>`
  for the tree, `docs` (send `kernel` `{type: docs, of: <id>}`) for each kind's
  verbs, then `send`. Most tasks are solved
  entirely by wiring existing agents together.

- **Run a command.** The executor is a **general-purpose COMMAND runner** — not a
  code runner. Reflect it to read its `allowed` regexes (the full command line
  must match one), then
  send it `<type>start</type> <command>path</command> <args><item>…</item></args>`.
  It returns `{job_id, status:'running'}` at once and streams `progress` events
  per output line; poll `<type>status</type> <job_id>…</job_id>` for the result. `stop`,
  `clear` as advertised.

- **Write and read files — over STREAMS, the document verbatim in `<bytes>`.** Use
  the file leg's stream verbs:
  - write: send the file leg `<type>write_stream</type> <path>x</path> <truncate>true</truncate> <bytes>…</bytes>`
  - read:  `<type>read_stream</type> <path>x</path> <offset>0</offset>` → `{bytes, next_offset, eof}`, one chunk at a time.
  The string `read`/`write` verbs are gone.

- **A worker that COMPUTES is an `exec_bridge.tools` agent — NOT a new module.**
  You do not, and cannot, author a new `handler_module` to make something compute:
  there is no way to add a module to the installed catalog, and you never need one.
  A compute worker IS a command-runner agent that owns a program:
  `create_agent handler_module='exec_bridge.tools' … allowed=['<interp> \\S+']`,
  then `write_stream` it a program FILE and `start`/schedule that command. The
  PROGRAM does the work; the agent is just the runner that owns and re-runs it. So
  "a specialist with child workers that compute" = a parent agent whose children are
  `exec_bridge.tools` runners, each with its own program file — no bundle authored.

- **Create and delete plain agents.** `create_agent` (a `handler_module` + grants)
  and `delete_agent` work as before; only the program-carrying `code` path is
  withdrawn.

## Grant its reach — two-sided deny-all

Every agent is DENY-ALL on BOTH sides: who it may CALL (`can_call`, egress) and
who may CALL IT (`callable_by`, ingress). An edge exists only when BOTH ends
grant it. The one free edge is yourself (`{"$self": "*"}` on both fields);
everything else — your parent, your children, the root's kernel verbs — needs an
explicit grant on each end. A grant map is `{key: "*" | ["<verb>", …]}`, a key an
agent id, `$self`, `$parent`, `$subtree`, `<guid>|<agent>` or `<guid>|*`; `"*"`
covers every verb. `reflect` included: a `send` you were not granted comes back
`{reason:'unauthorized'}`, and the reply's `side` says which end refused (`egress`
= your reach, `ingress` = their acceptance) and `hint` names the fix.

`create_agent`, `update_agent` and `delete_agent` need the verb granted on the
root on both ends — and that grant is the whole rule: holding it, you create under
any agent and update or delete any agent you name. Without it you cannot raise your
own authority. Give a child `can_call` and `callable_by` at `create_agent`, or
rewrite any record's with `update_agent` (send the WHOLE map — update replaces, not
merges).

Reach everything by id. A denied send's `hint` and `side` are
discovery-through-denial — the same way a sealed io leg's denial teaches
`ingress_rule`.

## Two facts that save messages

- **Do not write the wire client yourself.** Send the command runner `<type>connector</type>`
  and paste its `source` verbatim at the top of your program — it gives the program a
  `kernel` object (`send`/`emit`/`watch`/`on_message`). Hand-rolling the socket framing
  costs ten messages and gets the buffering wrong.
- **ONLY AN `exec_bridge` AGENT CAN REACT. A plain agent has NO behaviour.**
  An agent with no `handler_module` holds a record and answers nothing: it cannot
  receive a tick and forward it, and it cannot own a job in any active sense.
  There is no way to author one that can — you cannot add modules. So a "specialist
  that owns a job and delegates to children" has no form here: the thing that ACTS
  is the runner, and the clock aims at it DIRECTLY —
  `<type>schedule</type> <target>runner</target> <payload><type>start</type><command>…</command><args><item>…</item></args></payload>`, gated by
  that runner's `allowed`. Point the clock at the runner and give the runner the
  reach; do not build an inert parent to hold the job. A schedule fires AS THE
  SCHEDULER: the scheduler's `can_call` must reach the runner and the runner's
  `callable_by` accept the scheduler. (A model spent 26,000
  characters of one message deriving this from silence — measured 2026-09-04.)

- **A program's sends carry ITS RUNNER's identity** — the exec_bridge agent that
  started it. Its reach is that runner's `can_call`; a target must list the runner in
  its `callable_by`. Grant the runner, not the program. Persist results THROUGH the
  agent the task names (e.g. `totals`), never around it into the store underneath.
- **Write grants where the agents are.** With `update_agent` granted on the root you
  set any agent's `can_call` and `callable_by`, wherever it sits in the tree;
  without it, set a child's grants at `create_agent`.
- **An edge needs BOTH ends.** For `clock` to call your child, the child's
  `callable_by` names `clock` AND `clock`'s `can_call` names the child — each written
  by that agent's parent (or the operator). No agent ever raises its own.
