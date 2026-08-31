# durable_memory — modeless durable key/value store

`set` / `read` / `append` / `keys` / `delete` on flat namespaced keys
(dotted convention: `domain.subject.attribute`); values are arbitrary JSON.
`get` is a reserved kernel verb — the read verb is `read`.

What the store is FOR lives in the agent's `description`, like any agent.

**No fs of its own.** The record names `file_bridge_id`; every byte moves
through that leg over the raw stream channel (`write_stream`/`read_stream`).
State is one YAML doc (`state.yaml` under the leg's root): human-editable,
greppable, git-diffable, read fresh on every call. A store with no leg wired
FAILFASTS on write and reads empty — never silent RAM.
