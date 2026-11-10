# `autofold` (E141) — thoughts, large replies, recalls and cut answers fold to anchors (#556)

Free: no model. A scripted OpenAI-compatible server (the thought channel as in E119)
plays the model for one AI agent on the PACKAGED host of `RUNTIME` (on both runtimes: an
`ai_chat.tools` chat naming a `vllm_backend` provider),
next to a `file_bridge` agent holding an 80 kB file. `context_window` is 262144, so a
page of that size is delivered inline.

- P1: the AI reads the big file. The reply (over 64,000 bytes) is whole in the request
  that carries it, and stored whole.
- P2: the next send shows the read reply as `[folded · read reply from files · ~N tokens
  · head: … · get: B]` and the two thoughts as `[thought ~N tokens · id N]`; none of their
  text. The AI then `recall`s row B by id: the reply holds the whole body again.
- P3: after the next answer the recall reply is `[folded · recall reply from ai · id B ·
  ~N tokens · get: …]`, and the store row B is still whole.
- P4: a cut answer after thinking shows as `[cut · you wrote … after thinking ~N tokens ·
  nothing ran · ACTION: … · full: N]` at once, in the correction turn, without the thought.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id autofold --lanes 2
```
