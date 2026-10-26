# task_registry — which agent or subtree holds which task

Verbs: reflect, add, assign, reassign, status, report, list, get. **Off by default**: a host registers this bundle only when `FANTASTIC_TASK_REGISTRY` is truthy, and an agent of it with no `task_store_id` in its record refuses every verb (`disabled`).

**What it is.** The task tree of a team of agents, and the audit of who holds each task and where it came from. It sits between agents and task sources (gh, jira, ...) as an extension, not an interface layer: it makes NO outbound call. `source` and `update_to` are opaque strings (`gh:owner/repo#12`, `jira:KEY-5`, anything); the LLM reads them to see where a task came from and where to push updates, then pushes them itself through that source's own agent. Nothing is synced automatically. `reflect` shows the agent tree; this registry is the task tree.

**An entry** is `{id, parent, source, update_to, holder, history, status, report}`. `parent` is another task id or null. `holder` is an agent id, `{subtree: <root agent id>}`, or null; it must name an existing agent. `history` lists every change of holder as `{from, to, by, at}` (`by` is the caller, null for the operator). `status` is one of waiting, running, blocked, done, failed, paused (default waiting). `report` is the last report `{from, state, result, at}` or null.

**Verbs.**
- `add {id, source, parent?, update_to?, holder?, status?}`: `update_to` defaults to `source`. Refused `exists` for a taken id and `not_found` for an unknown parent or holder. A holder given here is the first history item.
- `assign {id, holder}`: only a task nobody holds; else `already_held`.
- `reassign {id, holder}`: only a held task; else `not_held`. Agent to agent, agent to subtree and subtree to agent are all allowed; every step is kept in `history`.
- `status {id, status}`: a state outside the six is `bad_arg`.
- `report {id, from?, state?, result?}`: records the last report. `from` defaults to the caller and must be the caller or one of its descendants, else `not_in_subtree`.
- `list {parent?, holder?, status?}`: `{tasks:[entry]}` sorted by id.
- `get {id}`: the entry plus `children` (ids of the tasks whose parent is this id); an unknown id is `not_found`.

**Writers are branch leads.** The authority is the kernel call gate: only a lead (a subtree root) is granted the write verbs (`add`, `assign`, `reassign`, `status`, `report`) in its `can_call` on the registry, and the registry's `callable_by` lists it. A worker has no grant: it sends its lead the `report` message of `agentic_resources` and the lead writes the registry. The one rule inside the bundle is the `from` check of `report`.

**Storage is abstract.** The registry keeps its tables in the store agent its record names (`task_store_id`), through the store interface the agent memory uses (`append`, `read`). Each write appends one snapshot row `{kind:"task", task_id, content:<entry>}`; every read takes the latest row per `task_id`. The wiring is explicit, as for memory: the registry's `can_call` reaches the store and the store's `callable_by` lists the registry. A store is `ram_store` (lost at exit) or a durable one; one shared store for the agents and the registry is a CONFIG choice, never enforced here. The store keys rows by the calling agent, so two registries on one store never mix. Nothing is held in memory: a restart rebuilds from the store. A store that refuses or is unreachable gives `store_refused` with the store's own error; nothing falls back to RAM. Writes of one registry are serialized, so two leads cannot race.
