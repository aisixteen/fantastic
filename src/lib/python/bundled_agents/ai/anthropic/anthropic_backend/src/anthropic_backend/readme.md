# anthropic_backend — an LLM agent over Anthropic's Messages API

Talks to Anthropic's Messages API (`POST {endpoint}`, streamed). Per-client chat
threads, FIFO lock, and automatic context management (compaction / recall /
context_status). Conversation in its memory store (`memory_store_id`). **No agentic
loop** — one provider pass per `send`. Tool-calling is this system's own `<call>`
text: no `tools` array is sent; ai_core parses the calls out of the answer.

## The record

REQUIRED — refused at the write when missing, every missing field named (the whole
field list is this kind's `schema` in `reflect bundles=all`; `memory_store_id` is
required too, as on every AI agent):
- `endpoint` — the Messages URL, e.g. `https://api.anthropic.com/v1/messages`
- `model` — the model id, e.g. `claude-sonnet-5`

READ FROM THE API when left out — before a verb that runs a turn (`send`, `inquiry`,
`context_recall`) each one still absent is read from the model's entry,
`GET {endpoint with /messages replaced by /models}/{model}`, and WRITTEN BACK to the
record (reflect shows it; cleared with `null`, it is read again):
- `max_tokens` — a turn's output budget; left out, the entry's `max_tokens`
- `context_window` — the input window; left out, the entry's `max_input_tokens`

An API that does not answer refuses the verb (`bad_arg`, `arg` = the field), naming
the URL and what came back.

Optional: `temperature` (sent only when set), `thinking_budget` (extended thinking
per turn — at least 1024 tokens and below `max_tokens`, refused at the write
otherwise; unset, no thinking is asked for; a side call too small for it gets none),
and `price_input_per_mtok` / `price_output_per_mtok` (USD per million tokens, numbers
zero or more; no default price — see Usage). `caching` and `cache_primer_until` are
ai_core's, on every AI agent — see Caching.

## Caching

`caching` is ai_core's mode: `none` | `cleanup` (default) | `provider_cache`, and this
backend can run all three. `none` and `cleanup` send no marker: `system` a plain
string, no `cache_control`, the body as it always was.

`provider_cache` sends ephemeral `cache_control` markers (`{"type": "ephemeral"}`, the
5-minute TTL, the only one sent) on EVERY request — main turns and side calls such as
fold summaries alike — at most four:
- `system` as one text block carrying the marker (absent when the system string is
  empty, as always);
- the end of the primer row — the row `cache_primer_until` names, when it is in view;
- the end of the last fold (or clear) row in view;
- a top-level marker, which caches the conversation tail automatically.

A turn holding a marked row is sent as text blocks cut at the end of that row's text,
so the marker lands there and not at the end of the merged turn; the blocks join to
exactly the merged string. A row with no text carries no marker, and no block is blank
(the API refuses one). A primer and a fold on one row send one marker. Nothing else in
the body changes: order, merging, thinking and `max_tokens` are as in the other modes.

The cache is a server-side PREFIX match. A turn whose system block changed re-writes
from the change on: an agent added or removed changes the tree index in the system
block. A turn whose history before a marker changed re-writes from the first changed
byte on. A repeat within 5 minutes of the previous request reads the shared prefix.

`prompt_cache` is retired: `caching: provider_cache` replaces `prompt_cache: true`.
`reflect` shows `caching`, and `cache_primer_until` when it is set.

## Usage

Always on, whether caching is on or off. Each response's stream reports its counts:
`message_start` gives `input_tokens`, `cache_creation_input_tokens` and
`cache_read_input_tokens`; `message_delta` gives `output_tokens`. These accumulate per
agent since it was created, and they persist (#308, Rust #319): each change rewrites the
agent's own `usage.json`, beside its `agent.yaml`, with the running total, so a kernel
restart keeps them. A missing or corrupt file reads as zero; a write error is reported
in `usage_log_error` and does not fail the turn. The `reflect` reply carries them as
`usage`, with `usage_since_boot` beside it: the same counts over this kernel process
only. The `usage` verb returns the same objects: `{type: "usage"}`, or
`{type: "usage", reset: true}` to return them and then zero the since-boot counts
(`reset` must be a boolean). `{type: "usage_reset"}` returns the total, then zeroes it
for good: the counts and the `usage.json` file (rewritten as zeros). Deleting the agent
drops them too.

- `requests` — responses counted (side calls too)
- `input_tokens` — the uncached remainder of the prompt
- `cache_write_tokens` — prompt tokens written to the cache
- `cache_read_tokens` — prompt tokens read from the cache
- `output_tokens` — output tokens
- `prompt_tokens` = `input_tokens` + `cache_write_tokens` + `cache_read_tokens`
- `hit_rate` = `cache_read_tokens` / `prompt_tokens` (0 when there are no prompt tokens)
- `saved_tokens` = round(0.9 × `cache_read_tokens` − 0.25 × `cache_write_tokens`), in
  full-price input-token equivalents: a read costs 0.1×, a write 1.25× the base input
  price. Computed exactly, halves to even. It may be negative — a write that is never
  read costs more than no cache — and is never clamped.

With BOTH `price_input_per_mtok` and `price_output_per_mtok` on the record (USD per
million tokens):
- `cost_usd` = Σ over the buckets of (`input_tokens` × price_in + `cache_write_tokens`
  5-minute part × write_5m + 1-hour part × write_1h + `cache_read_tokens` × read +
  `output_tokens` × price_out) / 1e6. The long-context bucket bills at its own input/output
  prices, its cache rates scaled by that input price over the base one.
- `saved_usd` = Σ over the buckets of (read × (price_in − read) − write_5m × (write_5m −
  price_in) − write_1h × (write_1h − price_in)) / 1e6: what caching saved against the same
  tokens uncached. `saved_tokens` above is kept as the token-equivalent at the 0.1× / 1.25×
  convention, so it can differ from `saved_usd` for Sonnet and Opus 5.5 (0.05× reads).

Cache rates default per model from the base input price: a read 0.1×, except
`claude-sonnet-5-5` and `claude-opus-5-5` (and their dated ids), which read at 0.05×; a
5-minute write 1.25×; a 1-hour write 2× (`usage.cache_creation.ephemeral_1h_input_tokens`
of the API reply). `price_cache_read_per_mtok`, `price_cache_write_5m_per_mtok` and
`price_cache_write_1h_per_mtok` on the record (USD per million tokens) override them.

`cost_usd` and `saved_usd` need BOTH `price_input_per_mtok` and `price_output_per_mtok`;
without both they are absent. No base price is assumed.

## The key

`credentials: {store, ref}` names the API key in a `credential_store` — read before
each request and sent as `x-api-key` (with `anthropic-version: 2023-06-01`). No
`credentials`, no key header: the API answers 401 and the error says to name one.

## The stream

`system` is one top-level string (every system message joined); `messages` holds the
user / assistant turns, consecutive turns of one role merged. With `thinking_budget`
the thought streams as `thinking_delta`s, wrapped once in `<think>…</think>` (stripped
before the turn is stored; only the answer's calls run). The stream ends with
`message_stop`; one that ends without it did not finish. An `error` event is raised
with the API's own type and message.

## Failures, in words

- HTTP 401/403 → the API wants a key: name it in `credentials`
- HTTP 4xx/5xx → the API's own error body, up to 500 chars
- `stop_reason: max_tokens` on a main turn → a cut: raise `max_tokens` or lower
  `thinking_budget`; a bounded side call (a fold summary) reads it as its answer
- the stream drops, or ends without `message_stop` → the turn is not complete
