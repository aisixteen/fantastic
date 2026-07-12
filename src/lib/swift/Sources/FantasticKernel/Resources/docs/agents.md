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

(No UI/view bundles live in this host — it is headless. A frontend lives in
`ts/` and is served generically over the bridge; reflect a TS frontend
kernel from `ts/dist` if you need one.)

To learn ANY agent: reflect it with `readme=true`.
