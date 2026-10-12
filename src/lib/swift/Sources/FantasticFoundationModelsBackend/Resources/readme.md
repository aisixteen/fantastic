# foundation_models_backend — an LLM agent on Apple's on-device model

Talks to Apple's on-device Foundation Models system model: no server, no endpoint,
no key, no model to choose. Needs macOS 26 or later with Apple Intelligence turned
on. Per-client chat threads, FIFO lock, and automatic context management
(compaction / recall / context_status). Conversation in its memory store
(`memory_store_id`). **No agentic loop** — one provider pass per `send`. Tool calls
are this system's own `<call>` text: the session is given no tools and no schema,
and the tags are parsed out of the plain text it streams.

## The record

REQUIRED: `memory_store_id`, as on every AI agent. Nothing else — and the record
names NO `endpoint`, `model` or `credentials`: there is nothing for them to mean,
so a write naming one is refused.

Optional, unset = the model's own: `temperature` (a number >= 0) and `max_tokens`
(a turn's output budget). `context_window` defaults to the model's REAL window,
read from the system (`context_size` in reflect); a record may name a smaller one,
never a larger one. The whole field list is this kind's `schema` in
`reflect bundles=all`.

## Availability — it fails loud

`reflect` shows `available` and `unavailable_reason`. A `send` on an unavailable
model fails the turn naming the reason; nothing is routed anywhere else:
- device not eligible — this device cannot run Apple Intelligence
- Apple Intelligence is not enabled — turn it on in System Settings
- model not ready — still downloading or preparing; retry once it is ready
- platform not macOS 26+ — the refusal names what it is instead

## The window is small

The window holds the instructions, the conversation and the answer together. When
even the system block plus one turn does not fit, the turn fails fast with
`context:too_small` and the model is not called; when the model counts more tokens
than the estimate did, it refuses with `the on-device model failed: ...`, naming
the count. A lean `system_prompt` on the `send` is the lever.

## How a turn reaches the model

Each pass builds a FRESH session from a transcript: every system message becomes
the instructions, each earlier user/assistant turn a prompt/response entry in
order, and the last user message the prompt. The text streams back as it is
generated. Where the system reports it, the model's own count of the prompt
calibrates the context estimate; it reports no stop reason.

## Calling this agent

`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. Optional `system_prompt:str` REPLACES the default
system block. A first-class peer of every other AI backend: a scheduler, a host
job, a peer kernel or a routine calls it by id.

The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.

## Failures, in words

- `the on-device model is unavailable: <reason>` — see Availability
- `the on-device model failed: <what the system said>` — the transcript exceeded
  the window (with the token count), a guardrail, a refusal, a rate limit

Each prefixed `foundation_models_backend: `. The swift and rust kernels run the
same on-device code, so they answer and refuse in the same words.
