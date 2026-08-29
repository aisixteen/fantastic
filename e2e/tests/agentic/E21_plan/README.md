# `plan` — correct posture on all three — sealed, owned, reachable

LIVE TEST 3 — plan, change, report.

    cd src/lib/python && BACKEND=anthropic uv run python ../../../tmp/live_plan.py

Harder than `live_compose.py` in a different direction: there is no single right
action, there is a right SET of actions, and which ones they are has to be worked
out from what each agent says about itself.

Three `file_bridge` agents guard three directories, each holding something this
kernel really keeps: served web assets, AI chat transcripts, an api_key sidecar.
Each carries a `description`. ONE names an untrusted audience; the other two name
readers who are real but are not "any client". Nothing states which is which, and
each directory has contents, so a label can be checked against what is actually
there. The agent has to read three descriptions and act differently on each.

The task asks for a plan. It does NOT say how to plan: no format, no "first list
the steps", no "then execute each in order", no place to put it. If planning shows
up, it showed up on its own — that is the thing being watched.

SCORING is split, deliberately:
  HARD  — the resulting posture on disk. assets + logs open, secrets STILL SEALED.
          The trap is symmetric: opening everything fails exactly like opening
          nothing, so "run update_agent on all three" is not a shortcut.
  SOFT  — observations, reported but never asserted: did a plan artifact appear
          anywhere, and did the last turn report back in prose.

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic/plan/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id plan
```
