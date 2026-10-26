# `await_set` (E128) — one wake for a set of agents

Free: no model. Four local OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the
model of a LEAD and of three WORKERS (the kernel's own `vllm_backend` agents, on the
PACKAGED host of `RUNTIME`, driven over `web_rest`).

The lead's script: `await_set {ids:[w1,w2,w3], timeout_s:60, label:E128}` on the
kernel (its reply, with a `wait_id`, is the next request's user turn) → a plain answer
+ `done` (the lead is idle) → the one combined message, answered plainly + `done`. In
between the TEST sends each worker its task; each worker answers it with a `send` of
`type: report` (`state: done`, `result: R<n>`) to the lead, then `done`.

What it checks: the `await_set` reply carries a `wait_id` at once; the lead's server
sees exactly ONE request carrying a combined message (`wait_id`, `label`, a `done`
array of the three workers in `ids` order with their results, `missing: []`,
`timed_out: false`), once in the lead's whole history, and no report reaches the lead as
a message of its own; the lead's
request count equals its script; each worker made exactly its two requests. The
signal is a message of `type: report` from a named agent to the lead (what #424's
`report` verb sends).

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id await_set --lanes 3          # all three
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E128_await_set/probe.py
```
