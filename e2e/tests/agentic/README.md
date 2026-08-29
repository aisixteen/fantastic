# `agentic/` — a model doing real work in the substrate

Eight tests. Each hands a model a task and nothing else — no scaffolding that
teaches the answer, no hard-coded call sequence — and asks whether the substrate's
own self-description is enough to get the work done.

| test | what it proves |
|---|---|
| `build` | it authors a program and the program actually runs |
| `compose` | three agents composed, a value computed and stored |
| `plan` | correct posture on all three — sealed, owned, reachable |
| `dialog` | a multi-turn exchange that keeps its own state |
| `send_to` | it DISCOVERS an unnamed peer from the tree and acts on it |
| `ledger` | the double-fire trap — one coin, proven twice, via a named execution |
| `deadletter` | a send to a dead agent is retried by the kernel, mourned by the ledger |
| `user_actions` | proposed, clicked twice, bought once |

The last three measure **kernel contracts through a model** rather than through a
unit test. That is a deliberate second bar: the unit tests already prove the
ledger is correct. These ask whether a model can *find* and *use* it — because a
contract nobody can discover at the surface where it matters is a contract that,
in practice, does not exist.
