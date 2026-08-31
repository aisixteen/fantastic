> Durable memory is just `durable_memory` agents you mount anywhere in the tree.

# Memory — durable state, as agents, everywhere

Memory here is just agents. A `durable_memory` agent is a key-value store you
mount ANYWHERE in the tree: under the root for global memory, under any agent
for that agent's own local memory — as many as you want, long- or short-term,
plus component/UI state. It is modeless: one set of verbs serves facts to
remember, current state, anything — the key names its purpose
(`user.name`, `decision.db`).

For an LLM agent, the memory agents mounted under it are auto-loaded into its
context on boot — you do not fetch memory, it is already present; you only
`set` what's worth keeping, under a descriptive namespaced key. Mount one with
`create_agent handler_module=durable_memory.tools`; `reflect readme=true` on it
for the verb guide (read / keys / set / append / delete).

`durable_memory` owns no disk surface of its own: it persists THROUGH a gated
`file_bridge` referenced by `file_bridge_id`, over `write_stream`/`read_stream`
— so it needs a wired store (`readme=persistence`), and fails loud until one is
wired.
