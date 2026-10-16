# `agentic/` — a model doing real work in the substrate

Three tests. Each hands a model a task and nothing else — no scaffolding that
teaches the answer, no hard-coded call sequence — and asks whether the substrate's
own self-description is enough to get the work done.

| test | what it proves |
|---|---|
| `build` (E20) | it builds a program that actually runs |
| `plan` (E21) | correct posture on all three — sealed, owned, reachable |
| `docs_mechanics` (E22) | it finds a deep agent and learns how to call it from `docs`, cutting the tree as it goes |

`dialog`, `compose`, `ledger`, `deadletter` and `user_actions` are shared
scenarios under [`../scenario/`](../scenario/) (`E10`–`E14`); the last three
measure kernel contracts through a model rather than through a unit test.
