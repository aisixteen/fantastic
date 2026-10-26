# `agentic_advanced/` — structures that outlive the conversation

The hard end. Not "can a model call a verb" but "can it leave behind something
that keeps working after the builder walks away".

| test | what it proves |
|---|---|
| `subai` (E30) | a sub-agentic routine — a new AI, a new code agent, the clock at the AI, the multiplier in code — that still works once the conversation ends |
| `subai_graph` (E31) | the same build, scored as a GRAPH — edges, not just nodes |
| `graph_shape` (E32) | A/B: does naming the graph change the shape the model builds? |
| `inquiry` (E33) | it sends the human an inquiry, and the answer steers what it does |
| `self_complete` (E34) | it finishes a small task and ends its chain with an explicit `done` call |
| `multisubai` (E36) | three solvers, three oracles, one code — the harness builds and kicks off; the swarm solves |
| `multisubai_build` (E37) | the creator builds the three-solver team, sleeps, is woken by its conclusion, validates, deletes the fleet, stops |
| `chat_users` (E38) | one AI is the bot of a multi-user chat server — events over several channels and accounts, including look-alike names, one person on two accounts and a spoofed identity |
| `chat_memory` (E39) | the chat bot's long pre-written past folds at a small window (a fold per batch, then a fold of folds); live asks recall from it |
| `self_managed_groups` (E124) | a master AI runs up to `MAX_GROUPS` worker groups (worker AI + editor + runner) from its prompt alone — creates, checks and deletes each; the harness builds no group. Scored on the bound, orphans, every suite green and the master's prompt staying bounded; run at 1 and 3 to compare walls |
| `pause_resume` (E126) | FREE (no model, Python runtime): `pause` holds every message in order in the memory store, a kernel restart keeps the pause and the queue, `resume` releases the held messages in order after one resumed row, none lost or duplicated |

**Wiring here includes granting authority.** The substrate is deny-all: creating
an agent connects it to nothing, and a builder that only creates agents has built
a pile, not a graph. `subai`'s task says so explicitly — it did not always, and
the omission cost real runs that scored as model failures while the fixture was
withholding grants the task required. `selftest.py` exists because of that.

These are the most expensive tests in the suite: long idle windows, high turn
caps, and several model-driven agents alive at once.

E124 (self-managed groups) needs a model key and the polyglot clone. Without a
ready backend or without the clone it prints `E2E-SKIP` and run.py reports SKIP.
A real run needs `MAX_GROUPS=1` and then `MAX_GROUPS=3`, and an operator runs it,
not CI.
