# cabinet

A master AI splits a job into **cabinets**. Each cabinet is a **canvas of its own**,
a child of the canvas that spawns it, with one **worker AI**. The worker builds its
**equipment** on its cabinet's canvas, knows its peers, and reports its **status** up.
The master sets the tasks, monitors the cabinets, collects the results, and at the
conclusion deletes every cabinet canvas with everything on it (except the cabinets it
keeps). The LLMs decide how many cabinets, each worker's set-up and each worker's
equipment.

## The pattern

```
canvas_root                  THE MAIN canvas (the spawner; the manager's canvas_id)
├── cabinet_alpha            cabinet.tools, cabinet_role: cabinet   THE STATUS CELL (live html view; its button opens the canvas below)
├── cabinet_beta             …same shape…
├── canvas_cabinet_alpha     canvas.tools  THE CABINET'S CANVAS (made by canvas_root's new_canvas)
│   ├── <equipment>          exec runners, file bridges, helper AIs, panels (placed by `create`)
│   └── canvas_cabinet_alpha_loader   its page records
└── canvas_cabinet_beta      …same shape…

cabinet_manager              cabinet.tools, cabinet_role: manager   (made once, by the operator)
└── cabinet_alpha_worker     the worker AI (a child of its cabinet cell; an AI backend's copy)
```

- **open_cabinet** `{task, name?, backend?, setup?}` (the master's verb, on the manager):
  asks the manager's `canvas_id` (the spawning canvas) for a child canvas `new_canvas`,
  makes the cabinet cell and its worker, then wires every worker to every other. It does
  not start the worker: send the worker its first message (the task), with
  `send {target: <worker>, payload: {type: "send", text: ...}}`.
- **close_cabinet** `{id}` deletes one cabinet with its worker, and its canvas with all
  the equipment on it.
- **cabinets** lists the live cabinets with their status, canvas and equipment.
- **conclude** `{keep?: [ids]}` closes every cabinet not named in `keep`. A `keep` id is a
  live cabinet of this manager (kept whole) or a result on a cabinet's canvas (kept with
  its canvas). The manager stays. Any other `keep` id is refused before anything is
  deleted (`reason: "unknown_keep"`).
- **status** `{line, progress?, result?}` (the worker's report, sent to its own cabinet):
  stored on the cabinet, emitted live, and drawn by the cabinet's status cell.
- **create** `{record: {handler_module, id?, …}}` and **place** (the worker's verbs on its
  own cabinet canvas): equipment and views. Those are the only two verbs it holds there.
- **view** and **reflect** on a cabinet: its task, worker, canvas, status and equipment.

The manager's serial verbs (`open_cabinet`, `close_cabinet`, `conclude`) run ONE AT A
TIME per manager. Each one reads whole grant maps and writes them back, so two in
flight would overwrite each other's entries. A batch of three opens (one turn's three
tool calls) therefore leaves every grant in place. Status and equipment are not serial.

## Setting up the manager

The operator makes the manager once, with:

- `cabinet_role: manager`, and `canvas_id` — the canvas that spawns the cabinets (the
  main canvas, `canvas_root`, for the main canvas's cabinets);
- `can_call` on the root for `create_agent`, `update_agent`, `delete_agent`, and the
  root's `callable_by` naming the manager with the same verbs;
- `callable_by` naming the master AI.

`open_cabinet` wires the rest itself: the manager's `can_call` on `canvas_id` for
`new_canvas`, and the canvas's `callable_by` for the manager.

## Wiring: what every worker knows

On every open and close, each live worker is rewritten with:

- **a directory** (`cabinet_peers`): the cabinet id, worker id and task of each peer;
- **instructions** (`post_system_prompt`): that it is one of N cooperating workers,
  how to message a peer or the master, how to report `status`, and how to build on its
  cabinet's canvas. The post prompt rides AFTER the backend's own prompt, so the
  backend's prompt is never replaced;
- **grants**, both ways, for MESSAGES only (`send`, `reflect`): each peer worker and
  the master can message it, and it can message them. Nothing else passes between
  workers, so a peer cannot stop, clear or re-task another worker.
- **a worker's own grants**: `status` on its cabinet (its `$parent`), `create` and `place`
  on its cabinet's canvas and nothing else there, and itself. It holds NOTHING on the root.

The master calls each worker in full (monitor, re-task, stop) and reads each cabinet
(`view`, `reflect`). The manager calls each worker in full (monitor, nudge, stop). A
close removes the cabinet's ids from every grant (the kernel prunes grants that name a
deleted id) and rewrites the directories.

## Equipment

A worker builds on its cabinet's canvas with `create` (`record: {handler_module, id?,
…}`, the record's own fields; the canvas places it as a child of itself). A helper AI
made as equipment reaches its memory store the way every created AI does: its record
names `memory_store_id`, and the store grants it (`callable_by` on the store). Equipment
is reached by a grant its record or its peers write; a worker reaches only what its own
`can_call` names.

Equipment is not the worker's child: it lives on the cabinet's canvas, so the close
removes it with the canvas. The worker cannot grant itself anything (no `update_agent`
on the root), and it cannot place a child canvas on its own canvas (`new_canvas` is not
on its grant).

## The status cell opens the canvas

The cabinet's record is a live html cell on the main canvas. It carries `canvas` (the
node id of the cabinet's canvas), and its button opens that canvas: the cell calls
`fantastic.openCanvas(canvas)`, and the page switches to `?canvas=<node>`. The cell is
disabled while no canvas is named. `view` returns `canvas` for it.

## Keeping some cabinets and results

`conclude {keep: [ids]}` takes two kinds of id, and refuses any other loudly before it
deletes anything:

- **a cabinet** (`cabinet_<name>`): kept whole. Its canvas, worker and equipment stay.
- **a result**: an agent placed on a cabinet's canvas, at any depth. Its canvas stays,
  with the path from the canvas to the result. The cabinet's record and worker are
  closed, and every other piece of equipment on the canvas goes with them.
  A canvas's page loader (its `kernel_state` child, `<canvas>_loader`) is its own records:
  it is never deleted, and it is never a result, so `keep` refuses it.

Every other cabinet is closed, and its canvas goes with it.

## Ceiling

`max_cabinets` on the manager refuses an open loudly when that many cabinets are live
(`reason: "ceiling"`). Unset, the LLM decides how many cabinets there are.

## Existing backend by default

The worker is a copy of an existing AI backend's record (`backend=`, default the master
itself). The copy keeps the endpoint, model, credential reference and memory store, so
an existing backend's connection and key stay where their owner set them. `setup=`
overrides fields of the copy. A worker whose handler, endpoint or credential differs
from the backend's is flagged `new_backend` with a warning: creating a backend is
discouraged, and the docs say why.

## Limits and what is not built yet

- **The Rust host's canvas** (`new_canvas`, nested records, delete cascade) is
  `src/tui/src/kern/web.rs`. In the Python tests the canvas is a stub that makes child
  canvases, children for `create` (and admits an AI to the store it names, as the host's
  `make_ai` does), and acknowledges `place`.
- **Equipment reach is not granted automatically.** A canvas's `create` gives a helper AI
  its memory store and nothing else; the worker reaches other equipment once its own
  `can_call` (or a peer's) names it.
- **`place` addressing** on a cabinet canvas is taken as the canvas node id; the view
  path is verified only by the e2e (#136).
