# cabinet

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
- **view** and **reflect** on a cabinet: its task, worker, status and equipment.

## Wiring: what every worker knows

On every open and close, each live worker is rewritten with:

- **a directory** (`cabinet_peers`): the cabinet id, worker id and task of each peer;
- **instructions** (`post_system_prompt`): that it is one of N cooperating workers,
  how to message a peer or the master, how to report `status`, and how to create its
  equipment. The post prompt rides AFTER the backend's own prompt, so the backend's
  prompt is never replaced;
- **grants**, both ways, for MESSAGES only (`send`, `reflect`): each peer worker and
  the master can message it, and it can message them. Nothing else passes between
  workers, so a peer cannot stop, clear or re-task another worker.
- **a worker's own grants**: `status` on its own cabinet, `create_agent` on the root (its
  equipment), and its own `$subtree` (the equipment it made) and itself.

The master calls each worker in full (monitor, re-task, stop) and reads each cabinet
(`view`, `reflect`). The manager calls each worker in full (monitor, nudge, stop). A
close removes the cabinet's ids from every grant (the kernel prunes grants that name a
deleted id) and rewrites the directories.

## Equipment

A worker creates its equipment by sending `create_agent` to the ROOT, with
`parent_id: <its worker id>`. The kernel answers `create_agent` on the root only (a
kernel verb sent to any other agent is refused `not_here`), so the root grant
`create_agent` is the one each worker holds. The equipment is then reached through the
worker's `$subtree` grant. Its `callable_by` should name its worker (or its own
equipment) and nothing else. The kernel's baseline is deny-all, so an equipment agent
that names no other caller is reached by no peer. Deleting the cabinet deletes the
equipment placed under the worker with it.

**Limit, stated plainly (spec item NOT met):** the root's `create_agent` takes any
`parent_id`. A worker can place equipment under the root or under another cabinet, and
the close does not delete it, so conclude can leave it behind. The gate judges the verb
on the root, never the placement, and the kernel keeps no record of who created an
agent. The worker's instructions say to place under itself only. Closing that gap needs
a kernel rule on `create_agent` (a parent must be the caller or its subtree), or a
record of each worker's creations. Neither is in this bundle.

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
beyond that. The manager wires the rest as cabinets open.

**The ceiling.** With `max_cabinets` set, an open that would exceed it is refused loudly:
`{error, reason: "ceiling", ceiling, open}`, and nothing is made.

## Runtimes

Python is the reference (this bundle). The Rust port is `fantastic-cabinet`
(`src/lib/rust/crates/bundles/fantastic-cabinet`): the same verbs, refusals, grants and
wiring, linked into the `fantastic` binary and the canvas app. The Swift runtime has no
port yet: a Swift host has no `cabinet.tools` bundle, so a cabinet cannot be opened
there. The parity rule in CONTRIBUTING is met for Python and Rust only; the Swift port
is a separate change.

## What it does not do yet

- **Equipment placement is not scoped** (see Equipment): equipment a worker places outside
  its own subtree survives the close.
- **No end-to-end test here.** The three-cabinet run is tracked as #136.
- **The canvas panel** is a plain html view (`view.html`) of the cabinet's id. The
  canvas draws the cabinets as cells the master places.
