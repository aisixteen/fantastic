# agentic_resources — a registry of agent roles
Verbs: reflect, roles, role, spawn, report, fleet, set_limit, get_count. **Off by default**: a host registers this bundle only when `FANTASTIC_AGENTIC_RESOURCES` is truthy, and an agent of it with no `role_dirs` in its record refuses every verb (`agentic_resources is disabled`).

**Config is the record.** `role_dirs` (folders of role cards), `tiers` (`{tier: backend fields}`; a card names a tier KEY, never a model), `limits` (kept by `set_limit`). A role is any kind of agent, not only code.

**A role card** is one `*.md` file: a `---` header (`name`, `purpose`, `tier`, `grants`) then the directive text, which becomes the child's system prompt. `grants` is the child's `can_call`; the key `$registry` stands for this registry. Adding a file adds a role (cards are re-read on every call); a bad card is listed in `roles` `errors` and skipped. A generic `project_lead` card ships built in.

**`spawn {role, task?, name?, limits?}`** makes the agent as a CHILD OF THE CALLER and gives the caller `pause`, `resume` and `stop` on it (both ends of the grant), so a lead always controls its subtree. A `task` object is stored as the task line `{label, state, waits_on, result}`; read the whole tree with `reflect tree=all`. The child sets its own line with `report {state, result}` and its parent gets a `{type:"report", from, state, result}` message.

**Limits.** `set_limit {role?, max}` caps one role, or all agents in total; `max: null` clears. `get_count` counts ACTIVE agents only (task state running or waiting; paused, done, failed and blocked free their slot). `spawn limits:{total?, per_role?}` hands a sub-lead part of the caller's quota; a subtree never exceeds what it was given. Counts are read off the records (`spawned_by`, `role`), so a delete lowers them and a restart rebuilds them.

**No cleanup.** `report done` only sets a state: nothing is retired or deleted. `delete_agent` is the only way out.
