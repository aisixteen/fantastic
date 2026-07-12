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
(`user.name`, `decision.db`). Mount one with `create_agent
handler_module=yaml_state.tools mode=mem|data`; `reflect readme=true` on it
for the verb guide (read / keys / set / delete / replace / state_yaml).

`yaml_state` persists `state.yaml` THROUGH a gated `file_bridge` referenced
by `file_bridge_id` — so it needs a wired store (`readme=persistence`).
