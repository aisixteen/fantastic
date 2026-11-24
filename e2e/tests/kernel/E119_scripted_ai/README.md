# `scripted_ai` (E119) — an AI agent's call loop against a scripted model

Free: no model. A local OpenAI-compatible server (`e2e/mocks/vllm.py`, extended in
`probe.py` with vLLM's thought channel `reasoning_content`) answers from a fixed
script; the kernel's own `ai_chat.tools` chat (ai_core), completing through a `vllm_backend` provider, runs the chain on the
PACKAGED host of `RUNTIME`, driven over `web_rest`. Every record field the backend
needs is set explicitly (`endpoint`, `model`, `context_window`, `kv_slots`,
`kv_overflow` on the provider; `max_tokens` on the chat, #632), so nothing is read
from the server.

The AI's own `can` lines reach `memory`, `files`, its provider and `reflect` on
the root; nothing is written on a target.

The script: a `reflect` call on the root (its reply, the root's id and none of
its guid, is the next request's user turn) → a `file_bridge` `write` (the bytes land verbatim) → calls
written wrong, cut at its first foreign tag (#499: the compiler-style report comes
back, nothing after the tag is stored; nothing runs) → an envelope with text outside
a tag (its fault comes back) → a call only inside the thought (not dispatched; the
no-call reminder comes back; the stored history has no thought) → a plain answer with
`done` (no further request). A cut answer and a thought show in the next request as their
autofold anchors (`[cut · …]`, `[thought ~N tokens · id N]`, #556). The checks are listed in `probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id scripted_ai --lanes 3
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E119_scripted_ai/probe.py
```
