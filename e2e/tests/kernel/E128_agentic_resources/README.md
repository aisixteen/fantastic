# `agentic_resources` (E128) — a roles registry, driven by a scripted model

Free: no model. A local OpenAI-compatible server (`e2e/mocks/vllm.py`) answers from a
fixed script; the agents are the kernel's own `vllm_backend` agents (ai_core) on the
PACKAGED host of `RUNTIME`, driven over `web_rest`. The registry is OFF by default:
the probe turns it on with `FANTASTIC_AGENTIC_RESOURCES=1` and gives the registry
agent its config on the record (`role_dirs`, `tiers`).

Script 1 (base): a `project_lead` agent asks `roles`, spawns two `writer`s, the
third `spawn` is refused at the limit of 2 (`set_limit role=writer max=2`), and
`get_count` shows 2/2.

Script 2 (extended): a `project_lead` spawns two sub-leads with split limits
(`limits:{total:1}` each); a sub-lead cannot spawn past its quota; the leaves
`report done`; each sub-lead reports a condensed line and the top lead reports one
line for the whole; the task tree (`reflect tree=all`) shows every task line and
all agents are still there after completion (nothing is cleaned up).

Each agent is triggered one at a time and the probe waits for it to settle, so the
one shared script stays in order. The checks are listed in `probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id agentic_resources --lanes 3      # all three
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E128_agentic_resources/probe.py
```
