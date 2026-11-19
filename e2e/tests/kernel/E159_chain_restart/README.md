# `chain_restart` (E159) — a chain in flight survives a kernel kill (#726, #733)

Free: no model. Two local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model
of a LEAD and a WORKER; the agents are the kernel's own `vllm_backend` chats (ai_core) on
the PACKAGED rust host, driven over `web_rest`. An `exec_bridge` agent `ex` runs the
slow job. The kernel's memory store (its ledger and the AIs' rows) is a `sqlite_store`
(`stores/memory.db`), so what was flushed outlives the process.

The lead's chain runs a 30 s job on `ex` (its call is out); the worker sends the lead a
message, which is HELD on the lead's chain; then the kernel's process group is KILLED
(SIGKILL: no hook, no drain) and a new kernel is booted on the same folder.

Everything is records: the new kernel settles the calls the old process left in flight
(a `call` row with no `settle`) as dead letters, `cancelled`; every AI agent they
concern is told once, at its boot, in one notice turn:

- the lead: `[reply to <pid> · ex run] no reply will come → dead letter <pid>; resend or
  drop`, and `[from worker · dead letter <pid>] a message to you was cut …`;
- the worker (the held sender): `[reply to <pid> · lead send] no reply will come → …`,
  the same pid.

The lead's chain goes on: it resends the job and reads its reply. The checks are listed
in `probe.py`'s docstring.

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --exclude-runtime python --id chain_restart
```
