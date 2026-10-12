# 02 — compose two agents you did not create

`tmp/live_compose.py` · **never run**

## What it asks

The model must wire together two agents it did not create and did not configure,
using only what `reflect` tells it about them.

## Why it is fabrication-proof

The score is a **SHA-256 of a nonce** minted at test time with
`secrets.token_hex(8)`. In the script's own words: *"A model that says DONE without
running anything cannot pass: it does not know the number."*

This is the strongest scoring design in the set. There is no partial credit, no prose
to interpret, and no way to reach the right answer except by actually driving the
agents. Worth copying into future probes.

## Status

Written 2026-07-31. **No recorded run against any backend.** It predates the
`llamacpp` backend, so it has no `BACKEND=llamacpp` branch — add one (mirror
`live_plan.py`'s) before running it locally.

## Also unrun

`tmp/live_send_to.py` (2026-07-28) — event-drain discover-and-act. Superseded by
[01](01_dialog.md) within the hour and never run. Kept only for the fixture shape.

`tmp/token_cost.py` (2026-07-28) — `count_tokens` per pass plus cached-vs-raw
pricing, meant to demonstrate the quadratic input shape of an agentic loop. Never
run, so the cost claims in its docstring are unmeasured.
