> A durable program is a FILE you write plus a command that runs it. Compose the agents you have, stream a program's bytes to a file, run it through the command executor. There is no `code` field.

# Self-extension

**A durable program is a FILE plus a command — not a `code`-carrying agent.**

The old way — a program pasted into a `code` field as a JSON string — is GONE:
a whole program escaped into an envelope is exactly how a call gets lost to one
stray quote. There is no `code` field, no `interpreters`, no `shim`;
`create_agent` with `code` is refused, and the refusal says so.

The way that WORKS now, and it makes something that genuinely persists and
re-runs:

1. **Write the program to a file** on the file leg, its bytes carried over a
   stream so nothing is escaped — `write_stream {path, bytes:<raw>, truncate:true}`,
   or `send_binary` (same call, but write the program RAW in the LAST `bytes`
   field — nothing escaped; see the `send` docs).
2. **Run it** with the command executor: `start {command:'python',
   args:['<your file>'], …}`. The file is durable on disk, so the SAME command
   run again later re-executes the SAME program against whatever the inputs hold
   THEN — which is what "build a thing that re-runs" means.

The program reaches the other agents itself, over its control wire, exactly as
any agent does. Assemble these pieces and you have built something durable
without any `code` field at all.

## What you CAN do now

- **Compose the agents you were given.** `send('kernel', {type:'list_agents'})`
  for the tree, `reflect` each for its verbs, then `send`. Most tasks are solved
  entirely by wiring existing agents together.

- **Run a command.** The executor is a **general-purpose COMMAND runner** — not a
  code runner. Reflect it to read its `allowed` regexes (the full command line
  must match one), then
  `send('<executor>', {type:'start', command:'<path>', args:['<arg>', …]})`.
  It returns `{job_id, status:'running'}` at once and streams `progress` events
  per output line; poll `{type:'status', job_id}` for the result. `stop`,
  `clear` as advertised.

- **Write and read files — over STREAMS, never a JSON string.** Putting file
  contents in a payload string is the same escaping trap as `code`. Use the file
  leg's stream verbs:
  - write: `send('<file>', {type:'write_stream', path:'x', bytes:<raw>, truncate:true})`
  - read:  `send('<file>', {type:'read_stream', path:'x', offset:0})` → `{bytes, next_offset, eof}`, one chunk at a time.
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
grant it. The free edges are structural — an agent reaches and is reached by
itself, its parent (its creator), and its own children; everything else needs an
explicit grant on each end. `reflect` included: a `send` to a leg you did not
create comes back `{reason:'unauthorized'}`, and the reply's `side` says which
end refused (`egress` = your reach, `ingress` = their acceptance) and `hint`
names the fix.

The one thing you CANNOT do is raise your OWN authority: editing your own
`can_call`/`callable_by` is refused. But you are NOT limited to your children —
if you hold `update_agent` (for example on the root/`kernel_state`, a common
grant), you can **open ANY agent's `callable_by`** to accept a caller, including a
SIBLING that is not your child (`totals`, `readings`, `clock`). Opening ingress
grants no reach, so it is freely conferrable this way. So to let a worker you made
call `totals`: give the worker `can_call:{totals:[…]}` at `create_agent`, AND
`update_agent id='totals'` to add the worker to `totals.callable_by` (send the
WHOLE map — update replaces, not merges). Both ends granted, the edge exists. New
reach for YOURSELF still comes only from your creator or the operator.

Reach everything by id. A denied send's `hint` and `side` are
discovery-through-denial — the same way a sealed io leg's denial teaches
`ingress_rule`.

## Two facts that save turns

- **Do not write the wire client yourself.** Send the command runner `{type:'connector'}`
  and paste its `source` verbatim at the top of your program — it gives the program a
  `kernel` object (`send`/`emit`/`watch`/`on_message`). Hand-rolling the socket framing
  costs ten turns and gets the buffering wrong.
- **A program's sends carry ITS RUNNER's identity** — the exec_bridge agent that
  started it. Its reach is that runner's `can_call`; a target must list the runner in
  its `callable_by`. Grant the runner, not the program. Persist results THROUGH the
  agent the task names (e.g. `totals`), never around it into the store underneath.
- **Create the agents you will grant reach on as YOUR children** (`parent_id: <your
  id>`). `$children` then covers them, so wiring `clock → your specialist` needs no
  lateral grant. A specialist created under the root is NOT yours: you cannot confer
  reach on it (refused: not your child, not in your can_call), and repairing that
  costs a delete + recreate (measured 2026-09-02: twenty turns).
