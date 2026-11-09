# `deadletter` — a flaky call is retried by the kernel and mourned by the ledger

Whether the model lets the KERNEL retry — one send carrying a `retries`
budget — rather than re-sending by hand. Re-sending by hand produces the
right answer for the pump and no dead-letter row for the valve, which is
exactly the state a caller cannot reason about later.

## The scenario is DATA

`scenario.yaml` holds the task, the roster, the grants and the assertions. It is
the ONLY copy: the same file runs against all four kernels, so changing the task
moves every runtime at once. That is the only thing that makes four results
comparable — copies drift, and drifted results measure different things while
still looking like one table.

## Roles, not bundle names

The roster names a ROLE; each runner supplies whatever its kernel has:

| role | python | rust |
|---|---|---|
| `memory` | `file_bridge` data store | `file_bridge` data store |
| `flaky_plant` | `plant_probe` (test-only) | `plant_probe` (test-only) |
| `render_sink` | `cli` | `cli` bundle |

`plant_probe` is a TEST-ONLY bundle on every runtime (a rust feature `scenario-probes`, the `e2e/probes/scenario_probes.py` launcher that registers it
for python); a default build
carries none of it.

## Running it

```sh
cd src/lib/python && KERNEL=python uv run python ../../../e2e/runners/host.py deadletter
cd src/lib/python && KERNEL=rust  uv run python ../../../e2e/runners/host.py deadletter
```

…or the whole cross-product through the launcher:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --group scenario
```

## The deadline case (free, scripted)

`test.yaml` + `probe.py` sit beside `scenario.yaml` and run with NO MODEL: two
scripted OpenAI-compatible servers play a lead and a worker (the kernel's own
`ai_chat.tools` chats over `vllm_backend` providers), and the root record carries `send_timeout: 3`. The worker
is busy on a slow answer; the lead's message to it is HELD, then passes its 3 s
deadline. The probe proves, on python and rust alike:

- `status` answers at once on a busy agent, counts the held message in
  `others_pending` and names the sender in `pending_from`;
- the message is withdrawn into a dead letter: the lead is told
  `your message to worker hit its 3.0s deadline → dead letter <pid>; resend or drop`,
  and the worker's history holds a `withdrawn` row naming the pid;
- the lead resends (`<timeout>30</timeout>`) and drops the dead letter
  (`ack_deadletter`); its resend's reply states `(deadline 30s)`;
- the worker made exactly two requests (the slow one, the wake for the resend):
  the withdrawn message never got a pass of its own.

```sh
cd src/lib/python && CARGO_HOME=/Volumes/Warehouse/tmp/cargo-home uv run python ../../../e2e/run.py --only-free --id deadletter_deadline
```
