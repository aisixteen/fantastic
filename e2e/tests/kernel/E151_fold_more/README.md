# `fold_more` (E151) — the rest of the fold cycle: a thought by id, other verbs, caching none, a person's fold (#556, #532)

Free, Rust lane: no model. Scripted vllm plugs (`e2e/mocks/vllm.py` `ThinkingVllmMock`, vLLM's
`reasoning_content` channel) play the model for four `ai_chat.tools` chats on the PACKAGED host,
each with its own `vllm_backend` provider, next to a `file_bridge` agent (`files`: an 80 kB
file and a folder of 600 files) and an `exec_bridge` agent (`ex`). `context_window` is 262144, so
every body (over 64,000 chars = AUTOFOLD_AT, under the inline cap) is delivered inline. Every
fold check asserts both halves: whole in the request that carries it, the anchor in the next.

- T (`provider_cache`): the model's first thought is `[thought ~N tokens · id P]` in the next
  request; a `recall` of P returns it whole; the recall reply is `[folded · recall reply …
  id P …]` after the next answer and the thought text is nowhere in that request, still in the
  store; across all five requests the messages up to the last cache mark are byte-equal in the
  next request and the message count never drops.
- V (`cleanup`): an `exec_bridge` `run` reply (500 lines, the verb's cap) and a `file_bridge`
  `list` reply (600 entries) fold to `[folded · {verb} reply from {agent} …]`, recall by id gives
  them whole, the recall reply refolds, the store row stays whole. `history` is covered by a unit
  instead: its reply is a digest cut to what fits inline (about 25,000 tokens), so 16,000 tokens
  need about 450 rows; the rule is verb-blind (`fantastic-ai-core` `tests_autofold.rs`).
- N (`caching: none`): nothing folds, no thought anchor, no cache mark; the read reply and a recall
  reply stay whole in every later request.
- U: the person's `context_fold` over the two rows of one turn (a given summary): a fold row, a
  `[fold ID · 2 refs · L1] "title"` line in place of them in the next request, a large read reply
  after it folds as usual, the folded rows stay in the store (`recall`, `history {fold}`).
  The free twin of the paid E72_user_fold.

Run (when the owner runs e2e): `--only-free --id fold_more`.
