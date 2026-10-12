# emergence — does capability fall out of self-description?

The probes that test the project's actual claim: **an LLM given only `send` /
`reflect` and the kernel's own readmes discovers the system, wires it, and authors
durable capability — without the wiring ever being coded or stated.**

These are **not** unit tests and not part of `pytest`. Each one boots a real kernel
in a fresh `mkdtemp`, wraps the AI backend's provider in a Spy, fires exactly ONE
user turn, waits out an idle window, and then **scores by reading state** — never by
reading what the model claimed. They cost real inference (local or paid), take
minutes each, and are run by hand.

The scripts live in the gitignored `tmp/` (they are scratch, and they carry
API-shaped fixtures that churn). **These docs are the durable record**: each one
carries the design, the exact scoring rule, and every recorded run, so a probe can be
rebuilt from its doc alone.

## The probes

| doc | script | asks | last recorded result |
|---|---|---|---|
| [01_dialog.md](01_dialog.md) | `tmp/live_dialog.py` | find a store nobody mentioned, and use it | claude-opus-5 ✅ · gemma4:12b ❌ |
| [02_compose.md](02_compose.md) | `tmp/live_compose.py` | compose two agents you did not create | never run |
| [03_plan.md](03_plan.md) | `tmp/live_plan.py` | read three prose descriptions, work out who may read what, apply it | **gemma-4-12B ✅** · claude-opus-5 ❌ |
| [04_build.md](04_build.md) | `tmp/live_build.py` | plan and BUILD a program that really runs | harness proven, model run pending |
| [05_thinking.md](05_thinking.md) | `tmp/live_llamacpp.py` | does the thought channel survive the backend | ✅ |

## Rules these probes follow

Learned the hard way; each has a scar behind it.

- **Score state, never prose.** A model saying it did the thing is not evidence.
- **Mint the answer at test time.** `secrets`-generated values cannot be recalled
  from training, so a fabricated answer cannot land on them.
- **Make the shortcut fail.** Every probe has an obvious cheat; the scoring has to
  refuse it explicitly, or a wrong method scores as a right answer.
- **Never repair the model's output.** A harness that patches a malformed call is
  measuring its own patch.
- **Only assert LEGAL values.** `03` returned a false ✅ for months because it
  scored "not `allow_all`" as sealed, while the value the model invented was one no
  rule implements and every call to that agent was raising.
- **Test the harness before spending a run.** `04` ships a companion selftest that
  plants a hand-written reference solution and proves all checks can go green. Two
  probes have scored FAIL on a healthy system because the harness asserted the wrong
  surface.
- **Don't invent requirements the task never stated.** `04`'s first canary demanded
  the authored tool be a live service. The task asked for a program. The harness was
  grading against its own assumption.
- **HARD vs SOFT.** HARD checks are asserted. SOFT observations (did a plan artifact
  appear, how much planning language, did the last turn report back) are printed and
  never asserted — they are what you watch for emergence, not what you gate on.

## Running one

```sh
cd src/lib/python
BACKEND=llamacpp uv run python -u ../../../tmp/live_plan.py
```

`BACKEND` selects the backend bundle: `llamacpp` (local `llama-server`), `ollama`,
`nvidia`, `anthropic`. Use `-u` — without it Python block-buffers stdout to a file
and the transcript stays invisible until the process exits.

Env knobs: `IDLE` (silence before scoring — must exceed ONE turn's wall clock, not
the run's), `CTX`, `FANTASTIC_AI_MODEL`, `FANTASTIC_LLAMACPP_ENDPOINT`.

Local model setup for `BACKEND=llamacpp` is in `05_thinking.md`.
