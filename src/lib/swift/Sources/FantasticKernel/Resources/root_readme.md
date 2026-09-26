# This is a Fantastic kernel.

A tree of agents. One primitive — `send(target_id, payload) -> reply`.
One discovery verb — `reflect` — which every agent answers with its
identity, verbs, and state. `kernel` is an alias for the tree root.

## Reflect — the discovery verb

`reflect` returns the ADDRESSED agent uniformly (`{id, sentence,
display_name, description?, verbs?, ...state}`). Each reply also carries
`kernel_verbs` — the verbs the KERNEL answers (`create_agent`,
`delete_agent`, `update_agent`, `list_agents`, `ps`, `cancel`, `ledger`,
`ack_deadletter`, `kernel_start`, `shutdown_agents`, `kernel_stop`). They are advertised on every agent and answered only by
the root: send them to `kernel` and name the agent you mean in
`parent_id`/`id`/`task_id`. Three flags compose the reply:

- `tree=all|ids|none` (default `all`) — the subtree: nested / id-list / omitted.
- `bundles=all|ids|none` (default `none`) — the installable-bundle catalog.
- `readme=true|index|<key>` (default `false`) — this kernel's manual (below).

SCAN cheaply first (`reflect tree=ids bundles=ids`), then DRILL into one
agent at a time. Never pull every agent's readme at once.

## The manual is a keyed doc tree

This readme is a SHORT orientation. The full manual is split into keyed
topics you fetch on demand, so you read only what a task needs — and each
entry shows its ~token cost so you can budget BEFORE fetching:

- `reflect kernel readme=index` → the topic map: each `{key, desc, tokens}`
  (some topics nest sub-docs — shown as `children`).
- `reflect kernel readme=<key>` → one topic's body + its sub-docs' index,
  e.g. `readme=self-extension`, `readme=streams`, `readme=transports`.
- `reflect kernel readme=true` → this orientation.

The root reflect always carries `readmes` (the top-level topic index) and a
`docs` one-liner, so the map is in front of you from the first round-trip.
If you mean to BUILD, the first topic to read is **`self-extension`** — you
author your own durable tools here.

## Reach this kernel

`.fantastic/lock.yaml` holds `{pid}` — one kernel per dir. A live daemon
locks out the one-shot CLI, so go through its web surface
(`readme=daemon`). No daemon → the one-shot CLI works: `fantastic reflect`,
`fantastic <id> <verb> [k=v ...]`. Every wire in: `readme=transports`.
