# E17 · setup_ai — the set-up page writes an ai agent, and it answers

Free, CPU only: a scripted keyboard in a PTY (the shell's own rig), a shell built
with `--features anthropic` (backend linked, nothing hardwired), and the parity
harness's scripted Anthropic stub (`e2e/parity/stubs/anthropic_sse.py`) as the
agent's endpoint. The key is the stub's dummy `ANTHROPIC_API_KEY`, from the
environment; no real key or endpoint is ever used.

    cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id setup_ai

| run | proves |
|---|---|
| `init` | a store from the blueprint |
| `write` | `+ add an ai agent` → the four fields (+ endpoint) typed in → WRITE THE RECORD → `✓ written to …/docreader/agent.yaml`, the agent in the tree, the record on disk |
| `answer` | next start: `docreader` rehydrates; the human writes `hello`; the stub's scripted reply `stub-reply-42` is on screen; the stub saw ≥ 1 request |
