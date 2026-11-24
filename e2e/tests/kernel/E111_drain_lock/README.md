# `drain_lock` (E111) — one kernel per directory, and the operator's graceful stop

Free: no model. The packaged rust host, every call over `web_rest`, the `slow`
probe bundle as the call that is in flight:

1. **the lock** — `.fantastic/lock.yaml` names the serving pid; a second kernel
   started in the same directory exits non-zero naming it; the first keeps serving;
2. **the process verbs are the operator's** — `shutdown_agents` and `kernel_stop`
   over REST (the leg is an agent) are each refused
   `{reason: not_allowed, error: "<verb>: operator only"}`; the kernel keeps serving;
3. **the operator's stop** (SIGTERM: drain, then graceful shutdown) with a call in
   flight — new work is refused `{reason:"draining"}` while it drains, `ps` and
   `reflect` still answer, the call is cancelled after the quiet window and its
   REST caller gets `{cancelled:true}`, exit 0, port closed, the lock released;
4. **stale locks** — a `lock.yaml` naming a dead pid, and one naming a live pid
   that holds no lock, do not block the next boot;
5. **the dead letter** — a call `rest` cancels in flight is an unacked dead letter
   in `rest`'s own `ledger`, `ack_deadletter` retires it; then the SIGTERM stop
   with a call in flight once more settles the caller and exits 0.

The in-process python twin of step 3 (dead letter across a restart) is
`memory/E90_graceful_stop` (muted, #170); the one-shot CLI's lock is in the parity `cli` family.
A drain waits out the kernel's quiet window (10 s), so this runs ~30 s.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id drain_lock --lanes 3
```
