# `foreign_cut_give_up` (E140) — the stream cut and its give-up (#499)

Free: no model. Scripted OpenAI-compatible servers (the thought channel as in E119)
play the model for two AI agents on the PACKAGED host of `RUNTIME`: on the python lane two `vllm_backend` agents; on the rust lane two `ai_chat.tools` chats, each naming a `vllm_backend` provider (#521).

- A: a foreign `<invoke name="send">` followed by a loop of closers, 12 times: each
  answer is cut at the tag (never stored whole), the correction is the next request,
  and after exactly 9 cut turns the chain gives up (9 requests, none after).
- B: 8 cut turns, a real call, 8 cut turns, done: 18 requests, no give-up.
- C: a cut after thinking: the thought stays in the store (`has_thinking`) and is not
  in the next request.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id foreign_cut_give_up --lanes 2
```
