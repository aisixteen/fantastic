# cabinet

> **NOT YET PORTED TO THE CANVAS DESIGN.** The Python bundle (`src/lib/python/bundled_agents/ai/cabinet`, its readme) now makes each cabinet a child canvas of the manager's `canvas_id`, with the worker placing equipment there by `create` and `place`, and `conclude {keep}`. This Rust port still has the pre-canvas design (a `equip` verb, the root `create_agent` grant for the cabinet, no `canvas_id`). Port it before the e2e (#136). The lock table below is already keyed and released as Python's weak map is.


A master AI splits a job into **cabinets**. Each cabinet is a **subcanvas** with one
**worker AI**. The worker builds its own **equipment** inside its cabinet, knows its
peers, and reports its **status** up. The master sets the tasks, monitors the cabinets,
collects the results, and at the conclusion deletes every subcanvas with everything in
it. The LLMs decide how many cabinets, each worker's set-up and each worker's equipment.

## The pattern

```
cabinet_manager              cabinet.tools, cabinet_role: manager   (made once, by the operator)
├── cabinet_alpha            cabinet.tools, cabinet_role: cabinet   THE SUBCANVAS (live html view)
│   └── cabinet_alpha_worker ollama_backend (or any AI backend)     the worker AI
│       └── …equipment…      exec runners, file bridges, helper AIs, panels (the worker's children)
├── cabinet_beta             …same shape…
└── cabinet_gamma            …same shape…
```

- **open_cabinet** `{task, name?, backend?, setup?}` (the master's verb, on the manager):
  makes the cabinet and its worker, then wires every worker to every other. It does
  not start the worker: send the worker its first message (the task), with
  `send {target: <worker>, payload: {type: "send", text: ...}}`.
- **close_cabinet** `{id}` deletes one cabinet with its worker and all its equipment.
- **cabinets** lists the live cabinets with their status and equipment.
- **conclude** closes every cabinet. The manager stays.
- **status** `{line, progress?, result?}` (the worker's report, sent to its own cabinet):
  stored on the cabinet, emitted live, and drawn by the cabinet's view.
- **equip** `{handler_module, id?, setup?, can_call?, callable_by?}` (the worker's
  equipment, sent to its own cabinet): one agent placed under the worker, granted inside
  the cabinet only. See Equipment.
- **view** and **reflect** on a cabinet: its task, worker, status and equipment.

The manager's serial verbs (`open_cabinet`, `close_cabinet`, `conclude`) run ONE AT A
TIME per manager. Each one reads whole grant maps and writes them back, so two in
flight would overwrite each other's entries. A batch of three opens (one turn's three
tool calls) therefore leaves every grant in place. Status and equipment are not serial.

## Wiring: what every worker knows

On every open and close, each live worker is rewritten with:

- **a directory** (`cabinet_peers`): the cabinet id, worker id and task of each peer;
- **instructions** (`post_system_prompt`): that it is one of N cooperating workers,
  how to message a peer or the master, how to report `status`, and how to place its
  equipment with `equip`. The post prompt rides AFTER the backend's own prompt, so the
  backend's prompt is never replaced;
- **grants**, both ways, for MESSAGES only (`send`, `reflect`): each peer worker and
  the master can message it, and it can message them. Nothing else passes between
  workers, so a peer cannot stop, clear or re-task another worker.
- **a worker's own grants**: `status` and `equip` on its own cabinet (its `$parent`),
  its own `$subtree` (the equipment it placed) and itself. It holds NOTHING on the root.

The master calls each worker in full (monitor, re-task, stop) and reads each cabinet
(`view`, `reflect`). The manager calls each worker in full (monitor, nudge, stop). A
close removes the cabinet's ids from every grant (the kernel prunes grants that name a
deleted id) and rewrites the directories.

## Equipment

A worker places its equipment by sending `equip` to its OWN cabinet. The cabinet makes
the agent on the root with `create_agent`, which the cabinet holds (its `can_call` names
the root, the root's `callable_by` names the cabinet; the manager grants both at open).
The worker holds no grant on the root, so it cannot create anything there, and the
cabinet forces the parent:

- **the parent is always the worker.** A `parent_id` on `equip` is refused. The equipment
  sits under the worker, inside the cabinet, and the close deletes it with the worker.
- **the grants stay inside the cabinet.** Every key of `can_call` and `callable_by` must
  be `$self`, `$parent`, `$subtree`, the worker, or an agent already under the worker. A
  key that names anything else (the root, the master, the manager, a peer, a store, the
  user) is refused loudly as `bad_arg` and nothing is made.
- **`callable_by` defaults to the worker and the equipment's own subtree**
  (`{"$self": "*", "$parent": "*"}`), so the equipment is reached by its worker alone.
- **`setup` carries record fields only.** The wiring keys (`parent_id`, `callable_by`,
  `can_call`, `handler_module`, identity, `root`, `watch`, ...) are refused in `setup`.

Equipment of a worker is reached through the worker's `$subtree` grant. A peer worker,
the master or another cabinet reaches it only if the equipment's own `callable_by` names
them, and that name must be inside the cabinet, so nothing outside can.

## Connect to an existing backend (recommended)

By default a worker is the **existing AI backend** that sent `open_cabinet`: the worker's
record is a copy of the master's connection fields (handler, endpoint, model, context
window and sampling), its memory store, and the `credentials` reference it names. Its
key is read from the credential store at use time, under the same grant the master has.
`backend=<AI agent id>` picks another existing backend.

**Why we recommend connecting, and why creating a backend is discouraged:**

- **Credentials stay where their owner set them.** A worker never receives a key.
  It reads the key the backend already names, through the store's own grant.
- **Cost and limits stay with the owner.** A new endpoint is a new account, rate limit
  and bill. Every worker on an existing backend draws on the budget the owner already set.
- **Fewer moving parts.** An existing backend is already tested and already wired.

`setup={...}` overrides fields of the copy (model, window, sampling). A worker on a
different handler, endpoint or credential is allowed, but the result comes back with
`new_backend: true` and a warning, so the master sees it. Wiring keys (`can_call`,
`callable_by`, `post_system_prompt`, `view`, `delete_lock`, identity) are refused in
`setup`.

## Operator wiring

The manager needs three grants on the root, and the root must name it. Once, as the
operator (the operator's sends are not gated):

```
create_agent  parent_id=<root> id=cabinet_manager handler_module=cabinet.tools cabinet_role=manager
              can_call={"$self": "*", "<root>": ["create_agent", "update_agent", "delete_agent"]}
              callable_by={"$self": "*", "<master>": "*"}
              max_cabinets=<N>        # optional ceiling; absent = the LLM decides
update_agent id=<root> callable_by={"$self": "*", "cabinet_manager": ["create_agent", "update_agent", "delete_agent"]}
```

The master AI's `can_call` names the manager (`"cabinet_manager": "*"`), and its
memory store (as every AI's does). The master's own record is not changed by the operator
beyond that. The manager wires the rest as cabinets open: each cabinet's own grant on
the root (`create_agent`) and each worker's grants.

**The ceiling.** With `max_cabinets` set, an open that would exceed it is refused loudly:
`{error, reason: "ceiling", ceiling, open}`, and nothing is made.

## The canvas host

The canvas app (`src/tui`) has its own reach: `kern/reach.rs` grants every AI agent (any
agent naming a memory store) on both edges to every other one. That is god mode for a
worker, and it would undo the peer-only messages and the cabinet scoping above. So the
reach **does not reach a cabinet**: a cabinet, its worker and the worker's equipment (any
agent whose own record or ancestor carries `cabinet_role: cabinet`) are not holders, and
the reach writes no callable_by grant on them. The one grant it still writes for them is
the human's own egress, so `user` may call a cabinet's `view` and `watch` to draw its
panel. The cabinet's grants are this bundle's alone. The predicate is `cabinet_owned`
in `reach.rs`, pinned by its unit test.

Known residual: the reach and the manager both read and rewrite whole grant maps, and
the reach runs on every `added` event. A reach write to a shared holder (the master's
`can_call`, say) that lands during an open can still lose one entry. Nothing here
serialises the reach against the manager.

## The subcanvas (for the maintainer to accept)

**Decision needed.** The spec asks for a subcanvas (a peer canvas or a nested view). Here
a cabinet is ONE html cell: its record carries a live `view.html` (the status panel), and
it is drawn on the main canvas as that cell. It is not a peer canvas made by
`new_canvas`, and the canvas has one view for now and no nested views (`src/tui/docs/canvas.md`).

Consequence: equipment the worker makes (panels, terminals) has no canvas of its own to
appear on. Only the cabinet's status cell is drawn.

This reading is recorded here for the maintainer to accept, or to replace with a peer
canvas per cabinet (`new_canvas`, the worker and its equipment under it, and the status
cell on the main canvas). Until that decision is made, the html-cell reading stands.

## Runtimes

Python is the reference (this bundle). The Rust port is `fantastic-cabinet`
(`src/lib/rust/crates/bundles/fantastic-cabinet`): the same verbs, refusals, grants,
wiring and serial lock, linked into the `fantastic` binary and the canvas app. The Swift
runtime has no port yet: a Swift host has no `cabinet.tools` bundle, so a cabinet cannot
be opened there. The parity rule in CONTRIBUTING is met for Python and Rust only; the
Swift port is a separate change.

## What it does not do yet

- **No end-to-end test here.** The three-cabinet run is tracked as #136.
- **The subcanvas is an html cell**, not a peer canvas (see The subcanvas). Equipment
  has no canvas of its own.
- **The reach and the manager are not serialised** against each other (see The canvas
  host).
- **The canvas panel** is a plain html view (`view.html`) of the cabinet's id. The
  canvas draws the cabinets as cells the master places.
