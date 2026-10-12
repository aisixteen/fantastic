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
| `write` | `+ add an ai agent` → the wizard: page 1 → vllm → page 2: the URL, the model, a pasted key (kept plain in the throwaway home), kv_overflow, the advanced budget fields → CREATE & OPEN CHAT → `vllm-1` made and selected, its saved set-up and the agent's record on disk, the stored key the configured one |
| `answer` | next start: `vllm-1` rehydrates; the human writes `hello`; the stub's scripted reply `stub-reply-42` is on screen; the stub saw ≥ 1 request |
