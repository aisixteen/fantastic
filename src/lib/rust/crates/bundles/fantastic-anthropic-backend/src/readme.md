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
otherwise; unset, no thinking is asked for; a side call too small for it gets none).

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
