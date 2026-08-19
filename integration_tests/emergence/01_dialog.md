# 01 — discover a store nobody mentioned, and use it

`tmp/live_dialog.py`

## What it asks

The kernel holds a `yaml_state` memory agent (`notes`) that the task **never
mentions**. The model is asked a question whose answer it must put somewhere durable.
Nothing names the store, nothing gives its verbs.

To succeed it has to `reflect` the tree, notice the store, work out `set` from that
agent's own reflect, and write to it.

## Scoring

**HARD** — read the store afterwards. The value has to be there, under the key the
task named as the answer. Prose claiming it saved something scores nothing.

## Recorded runs

### claude-opus-5 — 2026-07-28 21:01 — ✅

```
passes: 3
notes.read('answer') → {"key":"answer","value":42}
RESULT: ✅ discovered + acted
```

Three passes: reflect, find, write. The soundest positive result in the set — the
assertion is a store read, so there is nothing to fake.

### gemma4:12b via ollama — 2026-07-28 — ❌

```
passes: 4 · value null · RESULT: ❌ nothing stored
```

The failure mode is precise and worth keeping. It reflected the root with
`readme:'memory'`, then targeted `yaml_state_685249` — **the AI agent's own chat
sidecar** (`mode: chat`) — read its sentence, and still wrote `answer=42` there
instead of on `notes` (`mode: data`). It found *a* memory agent, not *the* memory
agent, and the `mode` field that distinguishes them did not stop it.

## Caveats on that failure

It is a real failure mode but not a clean measurement. The script still defaults
`IDLE=25` despite its own comment recording that 25s truncated a ~60s/turn gemma4 run
and "was read as a model failure". It also does not set `think=false`, which on
ollama produced completions with reasoning text and empty content.

Re-run it against `BACKEND=llamacpp` before drawing any conclusion about the model —
the thought channel is separated properly there, and `IDLE` now defaults to 240 for
non-hosted backends.
