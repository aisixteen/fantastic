> Agents are recursive nodes — create/delete/update them; the on-disk layout.

# Mental model

Agents are recursive — an agent can own children. `create_agent
handler_module=<bundle>.tools` spawns one (as a child of whatever you call
it on); `delete_agent` cascades depth-first. On disk each agent is
`<dir>/agent.json` with children nested under
`<dir>/<children_dir>/<child>/`. The container name is declared config (the
root record's `children_dir`, default `agents`) — set `host_agents` here /
`web_agents` on a `web_loader` for a self-describing layout.

`create_agent` / `update_agent` / `delete_agent` are kernel SYSTEM verbs:
they appear in no agent's `verbs` map — send them to `kernel` (or to any
agent, to act on its subtree). A record with `delete_lock:true` refuses
delete (`{error, locked:true, id}`); clear it via `update_agent`.

# Work in flight — the task table (`ps` / `cancel`)

An agent is a NODE; a **task** is one in-flight **dispatch** — the running
thing. Tasks are the kernel's process table, and they form a TREE: a `send`
issued inside a handler is a CHILD of that handler's task. Two more system
verbs (also in no `verbs` map — send them anywhere):

    send(kernel, {type:'ps'})
      -> {tasks:[{id, agent, parent, verb, started_at}, ...]}

    send(kernel, {type:'cancel', task_id:'<agent>:<n>'})
      -> {cancelled:true, killed:['<id>', ...]}     # the task AND its subtree
      -> {cancelled:false, killed:[]}               # unknown / already finished

A task id (`"<agent_id>:<n>"`) is a pid. `cancel` kills the task and everything
it spawned, children first — a process group, not one coroutine. Whoever was
blocked on the cancelled `send` gets the reply `{cancelled:true}` (it does not
raise). Bundles that own REAL resources release them on the way down (a
`python_runtime` job's OS subprocess is SIGKILLed, never orphaned).

These are plain verbs, so YOU can use them: read `ps`, pick the task, cancel it
— that is how one agent stops another agent's runaway work.

For durable state across restarts, wire a persistence provider —
`reflect kernel readme=persistence`.
