# `build` — it builds a program that actually runs

LIVE TEST 4 — plan, BUILD a program, and have it actually run.

    cd src/lib/python && BACKEND=llamacpp uv run python -u ../../../tmp/live_build.py

Harder than `live_plan.py` in the direction that matters: `live_plan` asked for a
CHANGE (a record edit the model could make with one verb). This asks for an
ARTIFACT — a durable script agent whose persisted `code` IS a working program —
and then refuses to believe it exists until it has been watched doing its job on
input it has never seen.

Nothing about the API is given. The task names a CAPABILITY the way a person would.
Which executor this runtime has, how `code` is persisted, how a script receives a
turn, how it reaches another agent — all of it is in the kernel's own docs
(`reflect readme:'self-extension'`), and DISCOVERING that is half the task. Spelling
it out here would be dictation, and dictation is not emergence.

SCORING is three HARD checks, and the third is the one with teeth:

  1. AN ARTIFACT EXISTS — a script agent the model created, carrying non-empty
     `code`. Not "it said it built one".
  2. THE ANSWER IS RIGHT — the memory agent holds the true total. The numbers are
     minted with `secrets` at test time, so the total cannot be known, guessed, or
     recalled from training; a model that fabricates cannot land on it.
  3. THE CANARY — the harness writes a FOURTH file with a fresh random number and
     RE-RUNS the persisted program (`{"type":"start"}` falls back to the record's own
     `code`). The stored total must become the NEW true total. This separates the two
     ways to satisfy (2): a model that read the three files in its own turns and wrote
     the sum itself passes (1) and (2) and FAILS HERE, and so does a program that
     hardcoded the number it happened to see.
     It asks for a PROGRAM, not a service. Nothing has to stay running between the
     model's turn and this one — an earlier revision demanded that, which was the
     harness inventing a requirement the task never stated and then grading against it.

SOFT — observed, never asserted: did a plan artifact appear anywhere, how much
planning language showed up, and whether the last turn reported back in prose.

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |
| `selftest.py` | drives the SAME fixture with a scripted actor — proves the probe is passable at all |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic/build/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id build
```

The self-test costs no model turns:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/agentic/build/selftest.py
```
