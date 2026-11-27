# `foreign_cut_give_up` (E140) — the stream checker, its retries inside the pass and the harness stop (#499, #766)

Free: no model. Scripted OpenAI-compatible servers (the thought channel as in E119)
play the model for two AI agents on the PACKAGED host of `RUNTIME`: on both runtimes two `ai_chat.tools` chats, each naming a `vllm_backend` provider (#521).

A malformed attempt is re-asked inside its pass: the next request is the pass's first
request unchanged plus one `{attempt as written, report}` pair per failed attempt. After
`1 + CALL_RETRIES` = 3 attempts the harness stops the pass (`constants.rs`,
`CALL_RETRIES = 2`).

- A: the BOSS asks the AI; a foreign `<invoke name="send">` followed by a loop of closers
  on every attempt: 3 requests (requests 2-3 the re-asks, none after though the script
  holds more); the one stored assistant row is the harness's
  `<call name="stop"><reason>3 attempts in a row wrote malformed calls or no call, so the
  harness stopped the pass.</reason></call>`; no attempt, loop or report is stored; the BOSS is
  told that reason.
- B: nine passes in a row that each fail twice and then answer with a real call, then
  one that fails twice and ends with `done`: 30 requests, no stop (no count runs across
  passes); each pass's first request holds nothing of an earlier pass's failed attempts;
  the first request shows A's stored stop call.
- C: a malformed answer after thinking: the re-ask holds the attempt, not the thought;
  the stored row is the accepted attempt's, with its own thinking (`has_thinking`);
  nothing of the failed attempt is stored.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id foreign_cut_give_up --lanes 2
```
