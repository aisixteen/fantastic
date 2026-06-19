# yaml_state — a memory agent (durable state)

A YAML key-value store that survives the context boundary — a first-class agent
you mount EVERYWHERE: global (under the root) or local (under any agent), long-
or short-term memory, or durable component state. The `mode` meta picks the
discipline (same verbs either way):
- `data` — current state (component state, config, run params, selection); overwrite-in-place.
- `mem` — durable facts to remember (names, preferences, decisions); accrete keyed facts.

**Reach it by id with one `send`, and manage it with JUDGMENT** — save salient
facts, recall them later (even from a fresh context), update, and prune; don't
store trivia. Where your backend injects memory on boot it's already in your
context (read it, don't re-fetch); otherwise `read` / `keys` it on demand.

## When to use
- The moment the user tells you something worth keeping → `set` it on a `mem` agent.
- When durable state changes → `set` it on a `data` agent.
- Reading: `read` / `keys` on demand (or, if your backend injects memory on boot, it's already in context — don't re-fetch).

## Verbs
- `read {key?}` — value at `key` (whole doc if omitted).
- `keys {}` — list keys + sizes (the table-of-contents).
- `set {key, value}` — upsert one key.
- `delete {key}` — prune a key.
- `replace {doc}` — overwrite the whole store (`{}` clears).
- `state_yaml {}` — the whole store as YAML (the block injected on boot).

## Persistence — automatic (through the loader)
yaml_state owns NO disk surface of its own, and there is **nothing to wire**. It persists
`state.yaml` THROUGH the loader (`kernel_state`), which writes it to the one `.fantastic`
store it owns — landing the sidecar at `agents/<id>/state.yaml`, next to this agent's own
`agent.json`. Just `set` / `read`. If no store is wired at the root, writes **fail fast**
(no silent RAM) and reads return empty; a denied write is surfaced, not lost.

```
create_agent yaml_state.tools mode=mem
```

## Recipes
- Remember a fact → `set {key:"user.name", value:"Ada"}`.
- Save state → `set {key:"view.zoom", value:1.5}`.
- Reuse keys, don't duplicate → `keys` first; use descriptive namespaced keys
  (`domain.subject.attribute`).
- Self-contained values (the fact AND its why) →
  `set {key:"decision.db", value:"postgres — chosen over mysql for JSON support, 2026-05"}`.
- Prune → `delete {key}` or `replace {doc}`.
