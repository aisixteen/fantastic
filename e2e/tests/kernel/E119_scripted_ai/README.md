# `scripted_ai` (E119) — an AI agent's call loop against a scripted model

Free: no model. A local OpenAI-compatible server (`e2e/mocks/vllm.py`, extended in
`probe.py` with vLLM's thought channel `reasoning_content`) answers from a fixed
script; the kernel's own `vllm_backend` agent (ai_core) runs the chain on the
PACKAGED host of `RUNTIME`, driven over `web_rest`. Every record field the backend
needs is set explicitly (`endpoint`, `model`, `max_tokens`, `context_window`,
`kv_slots`, `kv_overflow`), so nothing is read from the server.

The script: a `reflect` call (its reply, with the root's guid, is the next
request's user turn) → a `file_bridge` `write` (the bytes land verbatim) → calls
written wrong (the compiler-style report + the broken envelope's fault come back;
nothing runs) → a call only inside the thought (not dispatched; the no-call
reminder comes back; the stored history has no thought) → a plain answer with
`done` (no further request). The checks are listed in `probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id scripted_ai --lanes 3      # all three
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E119_scripted_ai/probe.py
```
