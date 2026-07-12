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

For durable state across restarts, wire a persistence provider —
`reflect kernel readme=persistence`.
