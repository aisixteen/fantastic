> Durable memory is just `yaml_state` agents you mount anywhere in the tree.

# Memory — durable state, as agents, everywhere

Memory here is just agents. A `yaml_state` agent is a YAML key-value store
you mount ANYWHERE in the tree: under the root for global memory, under any
agent for that agent's own local memory — as many as you want, long- or
short-term, plus component/UI state. The `mode` meta is just discipline
(verbs are identical): `mem` = facts to remember (names, preferences,
decisions); `data` = current state (UI, hyperparams, selection).

For an LLM agent, the memory agents mounted under it are auto-loaded into
its context on boot — you do not fetch memory, it is already present; you
only `set` what's worth keeping, under a descriptive namespaced key
(`user.name`, `decision.db`).

A `yaml_state` agent owns NO disk of its own: it persists `state.yaml`
THROUGH the `file_bridge` named by its `file_bridge_id` meta, so mount it
BOUND to the store — `create_agent handler_module=yaml_state.tools
mode=mem|data file_bridge_id=store`. Until that's wired, `set` / `delete` /
`replace` FAILFAST (`file_bridge_id required`) rather than silently dropping
to RAM. `reflect readme=true` on the agent for its verb guide (read / keys /
set / delete / replace / state_yaml). Wire the store first —
`readme=persistence`.
