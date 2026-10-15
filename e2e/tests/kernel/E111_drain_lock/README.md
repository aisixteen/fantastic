# `drain_lock` (E111) — one kernel per directory, and a graceful stop over REST

Free: no model. The packaged host of `RUNTIME`, every call over `web_rest`, the
`slow` probe bundle as the call that is in flight:

1. **the lock** — `.fantastic/lock.yaml` names the serving pid; a second kernel
   started in the same directory exits non-zero naming it; the first keeps serving;
2. **the drain** (`shutdown_agents`, with a call in flight) — new work is refused
   `{reason:"draining"}`, `ps` and the root's `reflect` still answer, the call is
   cancelled after the quiet window and its REST caller gets `{cancelled:true}`,
   `ledger` holds it as an unacked dead letter, `ack_deadletter` retires it, work
   stays refused;
3. **`kernel_stop`** — acked `{type:kernel_stop, ok:true}`, exit 0, port closed,
   the lock released;
4. **stale locks** — a `lock.yaml` naming a dead pid, and one naming a live pid
   that holds no lock, do not block the next boot;
5. **`kernel_stop` with a call in flight** and no drain first — the caller still
   gets its settlement, exit 0.

The in-process python twin of step 2 (dead letter across a restart) is
`memory/E90_graceful_stop`; the one-shot CLI's lock is in the parity `cli` family.
A drain waits out the kernel's quiet window (10 s), so this runs ~30 s.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id drain_lock --lanes 3
RUNTIME=swift uv run python ../../../e2e/tests/kernel/E111_drain_lock/probe.py
```
