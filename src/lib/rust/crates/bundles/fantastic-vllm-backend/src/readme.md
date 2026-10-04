# vllm_backend — an LLM agent talking to a vLLM server directly

Talks to a vLLM server over its OpenAI-compatible chat surface
(`POST {endpoint}/chat/completions`) with no proxy in between. Per-client chat
threads, FIFO lock, and automatic context management (compaction / recall /
context_status). Conversation in its memory store (`memory_store_id`). **No agentic
loop** — one provider pass per `send`. Tool calls are this system's own `<call>`
text: run the server WITHOUT a tool parser (one swallows the text and the answer
comes back empty).

## The record

REQUIRED — nothing is guessed, and a record missing any of them is refused at the
write, naming every missing field (the whole field list, with defaults and hints, is
this kind's `schema` in `reflect bundles=all`; `memory_store_id` is required too, as
on every AI agent):
- `endpoint` — the server's `/v1` base, e.g. `http://127.0.0.1:18000/v1`
- `model` — the served model name, e.g. `qwen3.8-27b`
- `max_tokens` — the output budget of a turn (unbounded, a runaway turn decodes for a long time)
- `kv_slots` — how many conversations may be LIVE on this endpoint at once
- `kv_overflow` — what happens to the next one: `evict` (the least-recently-used
  conversation rotates out) or `reject` (the newcomer is refused with a retry time)
- `context_window` — the server's window (its `--max-model-len`), used exactly as
  set. A main turn whose `max_tokens` reaches it sends NO `max_tokens`: the server
  bounds the reply by what the prompt leaves (prompt + `max_tokens` past the window
  is an HTTP 400). A bounded side call (a fold summary) keeps its own bound.

Defaults, used only where the record sets nothing: `temperature` 0.2, `top_p` 0.8,
`top_k` 20, `reasoning_effort` medium (`low`|`medium`|`xhigh`), `thinking_token_budget`
16000, `kv_ttl` 1800 s, `connect_timeout` 5 s, `read_timeout` 900 s. Sent only when set:
`min_p`, `presence_penalty`, `frequency_penalty`, `repetition_penalty`,
`no_repeat_ngram_size`, `seed`, `stop`. Mind the server: under speculative decoding
vLLM answers `min_p` with a 400, and presence/frequency penalties measured worse
(looping, then sub-word garbage) — `no_repeat_ngram_size: 3` with
`repetition_penalty: 1.05` is the measured fix for a repeating thought.

The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.

## The KV-cache session table

vLLM's prefix cache makes a warm conversation answer several times faster; past the
server's KV capacity it preempts, and a preempted conversation re-reads its whole
prompt. `kv_slots` keeps the conversations this kernel drives under that ceiling, and
`kv_overflow` decides who gives way. ONE table per endpoint, shared by every
vllm_backend agent in the kernel; `reflect` shows it (`kv.live`, `kv.conversations`
least-recently used first, `kv.evicted`, `kv.rejected`).

It is bookkeeping, not memory: vLLM owns the cache, and a rotated-out conversation
that returns still hits whatever blocks vLLM kept. It sees only this kernel's
traffic, so size `kv_slots` under what the server really holds.

What keeps the cache hitting is a byte-stable PREFIX: an append-only history under an
unchanged system prompt. A conversation is identified by its system text + first user
turn; when compaction rewrites that prefix it is, correctly, a new conversation.

## Thinking

The thought streams as `reasoning_content` and is wrapped once in `<think>…</think>`
(parsed for tool calls, stripped before the turn is persisted). `thinking_token_budget`
is vLLM's hard stop — once spent, the model moves on to its answer — so it must stay
BELOW `max_tokens` (refused at the write otherwise). On a main turn an explicit budget
is sent as set and the default is `min(16000, max_tokens // 2)`; a smaller side call (a
fold summary) gets `min(budget, its max_tokens // 2)`; below 256 no budget is sent at all
— a few tokens of thinking is no thinking, and a model starved of it stops with an
empty answer. The value sent is `last_thinking_budget` in reflect.

## Streaming parts — animation, never action
Your output streams as PARTS while you write it — `text`, `thinking`, `call` — each
`part_start {stream_id, source, kind, name?, target?, to?, re}` → `part_delta {text}`
(batched 50 ms) → `part_end {ok}` | `part_abort {reason}`. A call part goes to its
addressee's inbox (each of `to`) once `<target_id>`/`<to>` has closed; every other part
goes on your own id, tagged with the caller's `client_id`. Deltas ride inboxes only —
the state stream carries a part's start / end / abort, never a delta. Nothing
dispatches from a part: a call without its `</call>` is not a call, and ONLY A
SUCCESSFUL GENERATION DISPATCHES — a pass that stops with a block still open sends
nothing, its earlier closed calls included. The message a call becomes carries the
`stream_id` it streamed as (`· stream <id>` on the receiving header), so a consumer
settles its draft on it; a call part that ended and saw no such message within 60 s
is FAILED.

A receiver stops you with `stop {stream_id?}`: the provider stream closes and the open
parts abort. A CUT pass leaves a marker row on top of your context, never a half
answer: `[stopped by <receiver>]` or `[model stopped generating]` (`… while thinking`
when cut inside a thought), then the visible text, then `[a <tool> call to <target> was
not sent]` for every call of the pass.

`rows {after?, limit?}` is a VIEW of your conversation exactly as you hold it in
memory — no store read, no transform; before your first turn you hold nothing
(`loaded:false`).

## The key

A door in front of vLLM may want one (`Authorization: Bearer <key>`). Name it with
`credentials: {store, ref}` — a ref in a `credential_store`, read before each request
and sent only to the ref's host. No `credentials` on the record, no header goes out,
so a vLLM started without `--api-key` works as it is.

## Failures, in words

- connection refused → vLLM is not listening yet (it loads for ~75 s after boot), or the tunnel is down
- no answer within `connect_timeout` → the instance is stopped, or the tunnel/port is wrong
- HTTP 401/403 → the door wants a key: name it in `credentials` (`{store, ref}`)
- the stream drops, or ends without a `finish_reason` → vLLM died mid-answer; the turn is not complete
- `finish_reason: length` → a cut (`last_stop_reason: max_tokens`), so the caller can continue
