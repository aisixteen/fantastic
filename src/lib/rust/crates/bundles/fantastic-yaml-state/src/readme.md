# yaml_state — a memory agent (durable state)

A YAML key-value store that survives the context boundary — a first-class agent
you mount EVERYWHERE: global (under the root) or local (under any agent), long-
or short-term memory, or durable component state. The `mode` meta picks the
discipline (same verbs either way):
- `data` — current state (component state, config, run params, selection); overwrite-in-place.
- `mem` — durable facts to remember (names, preferences, decisions); accrete keyed facts.

**Your memory is auto-loaded into your context on boot — read it, don't re-fetch.**

## When to use
- The moment the user tells you something worth keeping → `set` it on a `mem` agent.
- When durable state changes → `set` it on a `data` agent.
- Reading: it's already injected; only `read` / `keys` when you need a key not in context.

## Verbs
- `read {key?}` — value at `key` (whole doc if omitted).
- `keys {}` — list keys + sizes (the table-of-contents).
- `set {key, value}` — upsert one key.
- `delete {key}` — prune a key.
- `replace {doc}` — overwrite the whole store (`{}` clears).
- `state_yaml {}` — the whole store as YAML (the block injected on boot).

## Persistence — wired, not automatic
This agent owns NO disk of its own. It persists `state.yaml` THROUGH a `file_bridge`
named by its `file_bridge_id` meta, at the store-relative path `agents/<id>/state.yaml`
(wire `file_bridge_id` to the `.fantastic` store, so the sidecar lands next to the
agent's own `agent.json` — one store, no `.fantastic/.fantastic/…` double-nest).
`set` / `delete` / `replace` **failfast with `file_bridge_id required`** until it's
wired — never a silent drop to RAM. `read` / `keys` / `state_yaml` degrade to empty
when unwired. Disk-is-truth: read fresh each call (an external edit is seen next read).

## Recipes
- Remember a fact → `set {key:"user.name", value:"Ada"}`.
- Save state → `set {key:"view.zoom", value:1.5}`.
- Reuse keys, don't duplicate → `keys` first; use descriptive namespaced keys
  (`domain.subject.attribute`).
- Self-contained values (the fact AND its why) →
  `set {key:"decision.db", value:"postgres — chosen over mysql for JSON support, 2026-05"}`.
- Prune → `delete {key}` or `replace {doc}`.
