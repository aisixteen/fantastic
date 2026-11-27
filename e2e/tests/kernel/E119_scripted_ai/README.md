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
its guid, is the next request's user message) → a `file_bridge` `write` (the bytes land verbatim) →
one pass of three attempts, re-asked inside it (#766, `CALL_RETRIES = 2`): a call written
wrong (it fails at its first foreign tag; the re-ask is the same messages plus the attempt
as written and the compiler-style report; nothing after the tag is read; nothing runs),
then an envelope with text outside a tag (the second re-ask adds it and its fault), then a
call inside the thought beside a real one in the answer (accepted; the thought's call is
not dispatched; the stored history has no thought) → one pass of two attempts (#795): an
answer with no call, re-asked inside the pass with the no-call report, then a plain answer
with `done` (no further request). After
the accepted attempt no request and no stored row holds either failed attempt or a report.
A thought shows in the next request as its autofold marker (`<thought id="N" tokens="…"/>`,
#556, map-once) at the top of the next user-side message, never in an assistant message. The checks are listed in `probe.py`'s docstring.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id scripted_ai --lanes 3
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E119_scripted_ai/probe.py
```
