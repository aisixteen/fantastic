# E124 `self_managed_groups` — a master runs its own worker groups (#174)

One master AI finishes a queue of Exercism python tasks by running up to
`MAX_GROUPS` worker groups at once. There is **no group code**: a group is four
agents the master creates under one id prefix — `g<k>_worker` (an AI chat, which
takes the task through `send`) and, under it, `g<k>_worker_prov` (the provider its
`provider_id` names, which completes its passes), `g<k>_editor` (a `file_bridge`
over the task's folder) and `g<k>_runner` (an `exec_bridge` that may start the
task's suite), E43's runner trio — with the kernel verbs every AI already has
(`create_agent`, `send`, `delete_agent`), and deletes once the task is checked (the
worker, its subtree with it). The recipe is the master's `pre_system_prompt`
(`helpers/groups.py` `master_prompt`); the task list is not in it, so the prompt does
not grow with the queue.

**The harness creates no groups.** It copies the exercises into `queue/<task>/`
(solutions and docs left out, the instructions written as `README.task.md`), makes
the `queue` file agent, creates the master with its `create` line on itself (`who`
`g*`, of the chat and the provider bundles; every worker chat's `can` forced to the
memory store and the queue, its provider's to the memory store and the key store, so
the master hands a worker no line it does not hold; the provider, editor and runner
go under their worker), says "begin" and then only watches: it samples the live agent
ids on a timer into a `GroupLedger`. Workers report into the queue (`<task>/DONE.md`);
a worker's last reply wakes the master.

| check | passes when |
|---|---|
| bound held | no sample saw more than `MAX_GROUPS` groups live at once |
| no orphans | no `g<k>_…` agent is live at the end |
| suites | every task's own suite passes, rerun by the probe, test files untouched |
| master bounded | the master's largest pass input (prompt + conversation, chars) ≤ `MASTER_INPUT_CHARS_MAX` |

It also prints the model per role, the wall time, each group's lifetime, passes per
agent and — on a metered run — tokens, cache hits and cost (for the whole test: the
meter tags per test, not per agent).

## Run it

`MAX_GROUPS` is required and never defaulted. Compare one group against three on the
same queue, as two separate runs (each its own kernel and log):

```sh
cd src/lib/python
export BACKEND=anthropic ANTHROPIC_ENDPOINT=<…/v1/messages> ANTHROPIC_API_KEY=<key> \
       FANTASTIC_AI_MODEL=<worker model> FANTASTIC_AI_MODEL_MASTER=<master model> E2E_METER=1
MAX_GROUPS=1 uv run python ../../../e2e/run.py --id self_managed_groups
MAX_GROUPS=3 uv run python ../../../e2e/run.py --id self_managed_groups
```

The exercises come from the gym's clone at `tmp/repos/polyglot-benchmark` (see
`tests/coding_exercism/README.md`); `EXERCISES` and `QUEUE_SIZE` pick the queue.
`helpers/groups.py` `compare(wall_1, wall_3)` gives the speedup from the two walls.

Without a key (or a ready backend) or without the polyglot clone, the probe prints
`E2E-SKIP: <reason>` and exits 0, and run.py reports SKIP. An unset `MAX_GROUPS`
with a ready backend still fails loud.
