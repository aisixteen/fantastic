# `autofold` (E141) — thoughts, large replies and recalls fold to markers (#556); a malformed answer is re-asked, never stored (#766)

Free: no model. A scripted OpenAI-compatible server (the thought channel as in E119)
plays the model for one AI agent on the PACKAGED host of `RUNTIME` (on both runtimes: an
`ai_chat.tools` chat naming a `vllm_backend` provider),
next to a `file_bridge` agent holding an 80 kB file. `context_window` is 262144, so a
page of that size is delivered inline.

- P1: the AI reads the big file. The reply (over 64,000 bytes) is whole in the request
  that carries it, and stored whole.
- P2: the next send shows the read reply as its tag line and the default type's
  `<folded id="B" … head="…"/> unfold: <call …recall…>` marker, and the two thoughts as
  `<thought id="N" tokens="…"/>` markers at the top of the next user-side messages (no
  assistant message holds one); none of their text. The AI then `recall`s row B by id:
  the reply holds the whole body again.
- P2b (map-once, the free twin of E168 `unfold`): that marker is the type's template
  (`res/default/marker_folded_reply.txt`) filled with the stored row's facts, byte for
  byte, its unfold call `recall` with row B's id.
- P3: after the next answer the recall reply is a `<folded … verb="recall" …
  recalled="id B" …/>` marker, and the store row B is still whole.
- P4: a malformed answer after thinking (a foreign `<invoke name="send">` and 50 closers)
  is re-asked inside its turn (#766): request 2 is request 1's messages, then the attempt
  without its thought (read no further than the delta that held the foreign tag) and the
  `[malformed call — nothing ran]` report. The store holds neither.
- P5: the next send: no request holds the failed attempt, its thought or its report.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id autofold --lanes 2
```
