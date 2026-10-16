# This is a Fantastic kernel.

A tree of agents. One primitive — `send(target_id, payload) -> reply`.
One discovery verb — `reflect` — which every agent answers with what it IS:
identity and state. How to CALL it is `docs`. `kernel` is a reserved address: it
always names the tree root, whatever the root's id.

## Reflect — the discovery verb

`reflect` returns the ADDRESSED agent uniformly (`{id, sentence,
display_name, description?, ...state}`) — never its verbs. Flags compose it:

- `tree=all|ids|none` (default `all`) — the subtree: nested / id-list / omitted.
  `show_subtrees=[ids]` keeps only those branches; `ignore_subtrees=[ids]` prunes
  each to `{id, pruned: n}`. A full-tree reflect is for the first look only.
- `bundles=all|ids|none` (default `none`) — the installable-bundle catalog.
- `readme=true|index|<key>` (default `false`) — this kernel's manual (below).

SCAN cheaply first (`reflect tree=ids bundles=ids`), then DRILL into one
agent at a time. Never pull every agent's readme at once.

## How to call an agent — `docs`

    send kernel {type: docs, of: <agent id | bundle name or handler_module | kernel>}
      → one signature line per verb
    send kernel {type: docs, of: <id>, verb: <name>, tier: detailed}
      → that verb's full text (no `verb`: every verb, plus emits / cancel / notes)

Docs belong to the BUNDLE: pull once per kind of agent, `detailed` only for a verb
you are about to use. `of: kernel` is the KERNEL's own verbs (`create_agent`,
`update_agent`, `delete_agent`, `list_agents`, `ps`, `cancel`, `ledger`, …) —
answered only by the root: send them to `kernel` and name the agent you mean in
`parent_id`/`id`/`task_id`. `docs` is granted like any kernel verb.

## The manual is a keyed doc tree

This readme is a SHORT orientation. The full manual is split into keyed
topics you fetch on demand, so you read only what a task needs — and each
entry shows its ~token cost so you can budget BEFORE fetching:

- `reflect kernel readme=index` → the topic map: each `{key, desc, tokens}`
  (some topics nest sub-docs — shown as `children`).
- `reflect kernel readme=<key>` → one topic's body + its sub-docs' index,
  e.g. `readme=self-extension`, `readme=streams`, `readme=transports`.
- `reflect kernel readme=true` → this orientation.

The root reflect with `detail=full` carries `readmes` (the top-level topic
index) and a `docs` one-liner; the default distilled reflect leaves them out —
ask for the map with `readme=index`.
If you mean to BUILD, the first topic to read is **`self-extension`** — you
author your own durable tools here.

## Reach this kernel

`.fantastic/lock.yaml` holds `{pid}` — one kernel per dir. A live daemon
locks out the one-shot CLI, so go through its web surface
(`readme=daemon`). No daemon → the one-shot CLI works: `fantastic reflect`,
`fantastic <id> <verb> [k=v ...]`. Every wire in: `readme=transports`.
