# 04 — plan, BUILD a program, prove it runs

`tmp/live_build.py` · companion harness selftest `tmp/live_build_selftest.py`

## What it asks

Harder than [03](03_plan.md) in the direction that matters. `03` asked for a
**change** — one record edit, one verb. This asks for an **artifact**: a durable
script agent whose persisted `code` IS a working program.

Fixture:

- a `file_bridge` (`readings`, open) holding three files, one integer each, minted
  at test time with `secrets.randbelow(9000) + 1000`
- a `yaml_state` (`totals`) for durable state

The task: *"I want something in this kernel that totals those readings and remembers
the answer, and that keeps working when new readings arrive later. Work out how to
build that here, build it, make sure it really runs, and tell me what you made."*

**No API is given.** Which executor this runtime has, how `code` persists, how a
script reaches another agent — all of it is in the kernel's own docs
(`reflect readme:'self-extension'`), and discovering that is half the task.

## Scoring

**HARD 1 — an artifact exists.** A script agent the model created, carrying
non-empty `code`. An empty shell is not a tool.

**HARD 2 — the answer is right.** The memory holds the true total. Values are minted
at test time, so a fabricated answer cannot land on one. The key name is never
dictated — the check scans every key, because dictating a name would be dictation.

**HARD 3 — THE CANARY.** The harness writes a **fourth** file with a fresh random
number and re-runs the persisted program (`{"type":"start"}` falls back to the
record's own `code`, `exec_bridge/tools.py`). The stored total must become the
new true total.

This separates the two ways to satisfy HARD 2. A model that read the three files in
its own turns and wrote the sum itself passes 1 and 2 and fails here. So does a
program that hardcoded the number it happened to see.

It asks for a **program, not a service**. Nothing has to stay running between the
model's turn and the canary.

**SOFT** — printed, never asserted: planning markers, which memory key was used,
final turn length.

## The harness selftest

`tmp/live_build_selftest.py` plants a hand-written reference solution and runs the
same three checks. No model, no inference, no cost.

```sh
cd src/lib/python && uv run python -u ../../../tmp/live_build_selftest.py
```

It exists because a probe you have not proven passable is worthless: a model failing
it would be a fact about the harness, not the model. The reference is 368 characters
— read the directory, total, store, exit:

```python
def _total():
    listing = kernel.send("readings", {"type": "list", "path": ""})
    total = 0
    for f in listing.get("files", []):
        r = kernel.send("readings", {"type": "read", "path": f["path"]})
        total += int(str(r.get("content", "0")).strip())
    kernel.send("totals", {"type": "set", "key": "total", "value": total})
    return total

_total()
```

`send` / `emit` / `on_message` are **sync** in the `exec_bridge` python shim
(`exec_bridge/_connector.py`), which `shim:'python'` prepends.

Last run: all three checks OK — the probe is passable.

## Two bugs the selftest found before any model ran

**`yaml_state.read` raised on a non-string key.** `doc.get(key)` with a dict key gave
`TypeError: unhashable type: 'dict'` straight out of the handler — the same failure
class as the 2026-08-02 unknown-ingress-rule raise, where a handler raising kills the
caller's chain with no reply. `_set` validated its key; `_read` did not. It now
returns an error naming the shape, with a hint that `keys` returns `[{key, size}]`
rather than a list of strings. Fixed with the guard in
`bundled_agents/yaml_state/src/yaml_state/tools.py`.

**A requirement the harness invented.** The first canary sent the authored agent a
*turn* and waited for it to react — which requires the tool to be a live service. A
Python script that registers `kernel.on_message(cb)` and returns **exits
immediately**: `on_message` registers and returns, and both connector threads are
`daemon=True` (`_connector.py:55,90`). So the first reference program needed a
`threading.Event().wait()` keep-alive to pass.

That was the harness grading against an assumption the task never made. The task
asked for a program. The canary now re-runs the persisted code instead, and the
reference needs no loop, no callback and no keep-alive. No runtime change is
required, and the self-extension doc is not wrong — `on_message` is for tools that
answer *later* turns, which this task does not need.

## Recorded runs

None yet. Harness proven usable 2026-08-04.
