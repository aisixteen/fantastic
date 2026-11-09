# E142 · bounded_stop — a signal stop is bounded

Scripted, no model (`costs: free`). #562: a bounded stop. A stub model never answers, so a model call is in flight at every stop.

1. First boot creates the root AI, then stops.
2. (a) `SIGTERM` with the call in flight: the kernel's clean stop runs first (it waits the quiet period) and finishes inside the 15 s grace.
3. (b) `SIGTERM`, one second later `SIGTERM` again: the second exits at once.

**PASS** when every check holds:

| check | what |
|---|---|
| a0 | the lock and the run file exist while it runs |
| a1 | the model call is in flight (a POST reached the stub) |
| a2 | a clean stop exits 0 |
| a3 | it waited the quiet period (>= 9 s) |
| a4 | under the grace (< 15 s) |
| a5 | the lock is released |
| a6 | the run file is removed |
| b0 | the model call is in flight again |
| b1 | a second signal exits 1 |
| b2 | at once (< 3 s after it) |
| b3 | the lock is released |
| b4 | the run file is removed |
