# `subai` — it builds sub-agents that outlive the conversation

LIVE TEST 5 — build a SUB-AI that keeps working after you leave.

    cd src/lib/python && BACKEND=llamacpp uv run python -u ../../../tmp/live_subai.py

The probes so far all end when the model's turn ends. This one asks for something
that OUTLIVES the conversation: a small specialised agent, wired to compute children,
that a SCHEDULER drives on a recurrence — with no LLM in the loop at all.

The fixture is the concept, laid out but not assembled:
  - `brain`     an AI agent: one completion per send, no loop of its own
  - `clock`     a scheduler
  - `readings`  a file agent holding numbers minted at test time
  - `totals`    a durable memory agent

What the model must work out: that a *mind* is just an agent whose code drives the
AI agent and its children; that a python exec is a durable tool whose persisted `code`
IS the program; and that a recurrence is a `schedule` pointing at what it built.
None of that is stated. All of it is in the kernel's own docs.

SCORING — four HARD checks, and the last two are what make it more than probe 04:

  1. STRUCTURE   a sub-agent exists with at least one script-agent CHILD carrying
                 real code. Not "it said it built one".
  2. WIRED       a schedule exists whose target is something the model created —
                 the recurrence is aimed at its own work, not at a fixture.
  3. IT RUNS UNATTENDED
                 the harness stops watching, lets the clock tick, and the memory
                 acquires the true total. The model's turn is OVER by then: nothing
                 the model does by hand can satisfy this.
  4. THE CANARY  the harness then writes a NEW reading and lets the clock tick
                 AGAIN. The stored total must follow. This separates a standing
                 structure from a one-shot that happened to fire once, and it
                 catches a program that hardcoded the number it first saw.

SOFT — observed, never asserted: how many agents it created, whether it reflected
the docs before authoring, planning language, and the final report.

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |
| `selftest.py` | drives the SAME fixture with a scripted actor — proves the probe is passable at all |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic_advanced/subai/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id subai
```

The self-test costs no model turns:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic_advanced/subai/selftest.py
```
