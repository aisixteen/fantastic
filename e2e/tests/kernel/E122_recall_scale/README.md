# `recall_scale` (E122) — a fresh execution name vs the stored-call count

Free: no model. Run SOLO (`solo: true`): it measures latency.

The PACKAGED kernel of `RUNTIME`, driven over `web_rest`. A call that carries a
caller-chosen execution name (`X-Fantastic-Execution-Id`) the kernel does not hold
in RAM makes the kernel look the name up in its memory store before launching
(`recall_executions`). This test measures how that call's latency grows with the
number of `call` rows the store holds.

1. Fill: cheap `reflect tree=none` calls on a small agent (`pad`, a `ram_store`
   child of the `rest` leg), 16 client threads on keep-alive connections, up to
   each checkpoint 0 / 10 000 / 50 000 / 100 000 stored calls. One REST call stores
   more than one `call` row (the leg's `inbound_call` plus the dispatched verb), so
   the rows-per-call ratio is measured once and the fill sized from it.
2. Count: the memory store's `dump`, counting the kernel's own non-internal `call`
   rows (owner `<guid>|kernel`) — the rows `recall_executions` scans. Polled until
   the flush has caught up with the checkpoint.
3. Time (same keep-alive client): 20 calls each carrying a FRESH name (a uuid), 20 unnamed calls, one at a
   time; median and p95 ms of each.

Checks: the stored count reached every checkpoint (the default `ledger_cap`,
1 000 000 on python, rust and swift, prunes nothing below it), and the BOUND
`fresh-name median at 100k ≤ 3 × unnamed median at 100k + 5 ms`. Every number is
printed whether the bound holds or not.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id recall_scale --lanes 1
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E122_recall_scale/probe.py
```
