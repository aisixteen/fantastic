> Agents are recursive nodes — create/delete/update them; orient inside the tree.

# Mental model

Agents are recursive — an agent can own children. `create_agent
handler_module=<bundle>.tools` spawns one (as a child of whatever you call
it on); `delete_agent` cascades depth-first.

To orient inside the tree: reflect yourself, then walk `parent_id` up to
your ancestors or list children to reach what you own. `reflect tree=ids`
shows every id at once. From any container agent you can spawn children next
to existing ones, inspect them, or cascade one out.

`create_agent` / `update_agent` / `delete_agent` are kernel SYSTEM verbs:
they appear in no agent's `verbs` map — send them to `kernel` (or to any
agent, to act on its subtree).

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
it spawned, children first — a process group, not one future. Whoever was
blocked on the cancelled `send` gets the reply `{cancelled:true}` (it does not
raise). Bundles that own REAL resources release them on the way down (a
`exec_bridge` subprocess is SIGKILLed, never orphaned).

These are plain verbs, so YOU can use them: read `ps`, pick the task, cancel it
— that is how one agent stops another agent's runaway work (and how an AI turn
is stopped: it is just a `send` task on the backend agent).

(No UI/view bundles live in this host — it is headless. A frontend lives in
`ts/` and is served generically over the bridge; reflect a TS frontend
kernel from `ts/dist` if you need one.)

To learn ANY agent: reflect it with `readme=true`.
