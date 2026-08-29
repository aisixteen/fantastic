# `llamacpp` — llama-server round trip — streaming, thinking markers, tool envelope

SMOKE — the llamacpp_backend bundle against a live `llama-server`.

    cd src/lib/python && uv run python ../../../tmp/live_llamacpp.py

Not an emergence probe. This asserts the ONE thing the bundle exists for: that a
turn spent reasoning is not read as an EMPTY completion. Two sends against the
same agent, differing only in the `think` record field:

  think=False  → no thought channel; `content` carries everything.
  think=True   → the model reasons, and BOTH channels survive: the thought arrives
                 wrapped once in <think>…</think> (parsed for tool calls, stripped
                 before persistence) and `content` is still non-empty.

The failure this guards against is silent: with the gate off and a provider that
reads only `content`, the model spends a whole pass thinking and you observe
nothing at all — no error, no text, no turn.

## Files

| file | what it is |
|---|---|
| `probe.py` | the runner — builds the tree, drives the model, scores it |
| `kernel.yaml` | the agent roster this test builds, declared |
| `test.yaml` | limits and knobs; **backend-agnostic by construction** |

Run it alone:

```sh
cd src/lib/python && uv run python -u ../../../e2e/tests/backend/llamacpp/probe.py
```

…or through the launcher, which picks the backend and the lanes:

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --id llamacpp
```
