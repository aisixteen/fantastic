# E17 · setup_ai — the set-up page writes an ai agent, and it answers

Free, CPU only: a scripted keyboard in a PTY (the shell's own rig), a shell built
with `--features vllm` (backend linked, nothing hardwired), and the parity
harness's scripted OpenAI-chat stub as the agent's endpoint (its `/v1` base).
Keyless — a vLLM record's key is optional; the keyed path (`k` → the credential
store) is E20's. No real key or endpoint is ever used.

    cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id setup_ai

| run | proves |
|---|---|
| `init` | a store from the blueprint |
| `write` | `+ add an ai agent` → the id, the handler and vLLM's required fields (endpoint, model, max_tokens, kv_slots, kv_overflow) typed in → WRITE THE RECORD → `✓ written to …/docreader/agent.yaml`, the agent in the tree, the record on disk |
| `answer` | next start: `docreader` rehydrates; the human writes `hello`; the stub's scripted reply `stub-reply-42` is on screen; the stub saw ≥ 1 request |
