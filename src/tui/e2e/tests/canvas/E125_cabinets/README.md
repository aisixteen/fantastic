# E125 `cabinets` — a master opens 3 cabinets, the workers build and talk, the master concludes

#136, the end-to-end of #132 on the REAL host (the canvas tree, the reach, the page). Free: the vLLM mock
from `e2e/mocks` (no model); the master and the three workers share ONE mock and are scripted by their
system block (a worker's carries `worker AI of cabinet cabinet_<name>`). The master is an AI cell opened
the way a person opens one; the manager (`cabinet.tools`) is seeded under the main canvas and the root
names it, as the cabinet readme's "Setting up the manager" says. Each step is a message the test sends
the master or a worker; a scripted AI answers each with the calls that step is about.

1. C1 open: the master's ONE turn holds three `open_cabinet` calls. Exactly 3 cabinets, 3 workers, and 3
   canvases that are children of the main canvas; each worker copies the master's backend (endpoint, model,
   store) and is no new backend; each knows exactly the other 2 cabinets and workers; it may only
   `send`/`reflect` its peers and the master, `create`/`place` on its own canvas, `status` on its cabinet,
   and holds nothing on the root; its canvas holds `create_agent` on the root and nothing else. The master
   then `place`s the three status cells on the main canvas (live, sandboxed panels);
2. C2 equipment: each worker `create`s an exec agent on ITS OWN canvas. The person grants the worker its
   equipment (no grant is automatic, see the readme's limits); the worker then starts `echo` and reads it
   back through `tail`. Beta's attempt on alpha's equipment is refused (`may not call 'start'`);
3. C3 talk: alpha sends beta a ping; beta's turn carries it and beta reports `got ping from alpha` up;
4. C4 status up: every worker reports `working` and later `done`. A probe in each cell's own frame (the
   panels are out-of-process) keeps every line it shows: both, in order, no reload of the page (a window
   marker and the navigation count survive), and no polling (the cells' `view` calls stay within one per
   update);
5. C5 monitor: the master's `cabinets` reply carries the same lines and canvases; the master re-tasks alpha
   (its next turn carries the new text and its cell shows the line it reports, live) and stops beta while
   its model turn hangs (idle, no further model turn);
6. C6 conclude: `conclude {}` closes all 3. No cabinet, worker, canvas, page loader or equipment is left in
   the kernel or under `.fantastic`; the manager, the master and the main canvas remain; no surviving
   record's grants name a closed id; a closed cabinet answers nothing; the three status cells are gone;
7. C7 no page error and no host panic.

## What it found (fixed with it)

- the cabinet's canvas was made BEFORE its record, so the host's reach took it for an ordinary canvas: a
  holder with `*` on the root, the human and every AI. Now the record (naming the canvas) comes first and
  the canvas gets `create_agent` on the root, which is all its worker's `create` needs;
- the reach read the cabinets BEFORE it listed the holders, so a worker made in between was a holder for
  one grant (`kernel` and more in its `can_call`): the cabinets are read after;
- `new_canvas` named its loader in `user.can_call` by reading the map whole and writing it back, losing a
  grant the reach wrote at the same time (the person lost reach to a cabinet): now one merge under the lock.

## Known limits

- the scripted replies cover the wiring, the grants, the live status and the teardown. How many cabinets a
  real master opens, what equipment a real worker builds and whether it reports honestly is the model's
  judgement: the live tier's, not this one's;
- a worker reaches its equipment only once a grant names it (the person's step here): there is none
  automatic, as the cabinet readme says.
- a worker's `place` on its cabinet's canvas is answered `canvas: unknown type "place"`: the host's canvas node
  has no `place` verb (views are placed on the page's view, which a worker holds no grant on). The probe
  records the answer (`C2_alpha_place_on_its_canvas` in summary.json) and checks nothing on it: how a worker
  shows a view is open (the cabinet readme leaves the view path to this e2e).
