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
- **grants**, both ways: every peer worker and the master can call it, and it can call
  them (`send` for messages, `status` on its own cabinet, `create_agent` on the root for
  its equipment, and its own `$subtree` for the equipment it made).

The master can call each worker and read each cabinet (`view`, `reflect`). The manager
can call each worker (monitor, nudge, stop). A close removes the cabinet's ids from every
grant (the kernel prunes grants that name a deleted id) and rewrites the directories.

## Equipment

A worker creates its equipment as its own children: `create_agent` with
`parent_id: <its worker id>`. The equipment is then reached through the worker's
`$subtree` grant. Its `callable_by` should name its worker (or its own equipment) and
nothing else. The kernel's baseline is deny-all, so an equipment agent that names no
other caller is reached by no peer. Deleting the cabinet deletes the equipment with it.

**Limit, stated plainly:** the kernel gates `create_agent` by verb on the root, not by
parent. A worker holding `create_agent` on the root could name another cabinet's agent
as `parent_id`. The worker's instructions say to create under itself only. Refusing it
in the kernel would be a new rule on the create verb, not in this bundle.

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

## What it does not do yet

- **No Rust or Swift port.** This bundle is Python, the reference runtime. The verbs
  (`open_cabinet`, `close_cabinet`, `cabinets`, `conclude`, `status`, `view`) are new, so
  the parity rule in CONTRIBUTING applies. A port is a separate change.
- **No end-to-end test here.** The three-cabinet run is tracked as #136.
- **The canvas panel** is a plain html view (`view.html`) of the cabinet's id. The
  canvas draws the cabinets as cells the master places.
