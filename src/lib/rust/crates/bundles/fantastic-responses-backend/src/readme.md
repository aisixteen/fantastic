# responses_backend — an OpenAI Responses-API, context-managed completion agent
Talks to any endpoint that serves the OpenAI Responses API (`POST …/v1/responses`): OpenAI itself, Ollama ≥ 0.13.4, or a proxy in front of an open-weights model. `endpoint` (the full `…/v1/responses` URL) and `model` are REQUIRED on the record — nothing is guessed; `credentials` is REQUIRED too: the key is read before each request from the record's `credentials: {store, ref}` — a ref in a `credential_store`, sent only to the ref's host — never from the environment or the record. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Conversation in its memory store (`memory_store_id`).

## Implementation
This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `ResponsesProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

## The wire
Stateless: every pass sends `store:false` and the whole conversation as `input` items (the system block as `instructions`) — the conversation is this agent's, never the endpoint's. The stream is read on BOTH channels: `response.reasoning_text` / `reasoning_summary_text` deltas are wrapped in `<think>…</think>` (stripped before the turn is stored; only the answer's calls run), `response.output_text` deltas are the output. A turn ends `completed`; one cut at `max_output_tokens` (`response.incomplete`) is a loud error on a main turn and an answer (`last_stop_reason:"max_tokens"`) on a bounded side call such as a fold summary. Sampling knobs on the record (`sampling`) merge verbatim into the request body.

## Bare completion + the `<call>` grammar — no native tool API
`send` performs exactly ONE provider pass: the stream's text is read; it never sends Responses `tools` nor reads `function_call` items (one arriving is a loud error). Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

## Context management — owned HERE
The conversation lives in this agent's MEMORY STORE — `memory_store_id` on the record
is REQUIRED (a `send` fails loud naming it): every turn, fold and view is a row under
this agent's key. Reply BODIES are never stored — a reply row keeps its one-line
`origin`, and after a restart the model re-reads from it. Compaction fires at 0.8 of
the budget (calibrated by the endpoint's reported prompt size when it reports one)
with the per-agent `context_strategy` (default `compact`; `fold`, `truncate`) and a
`[context-notice]`; `context_status`, `context_recall`, `history {fold?}`, `grep` and
`context_clear` (a boundary, nothing deleted) inspect and page it; `context_forget`
drops the rows folds already summarise. `watch {body?}` /
`unwatch` subscribe to this agent's row events (a digest; with `body` the content too).
## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
REPLACES the default system block (the caller supplies its own role / tool convention).
The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.
There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text.

## Streaming
You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
consumes the same stream (the driving caller, a browser tab, a peer). You do not route
your own output — you complete; the caller reads it and decides what to do next.

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

## Caching

`caching` is `none` | `cleanup` (default) here; `provider_cache` is refused — it is
Anthropic's, and the Responses API gets no cache marker (#171). `none` keeps the history
append-only (folds still compact it), which suits a server that caches identical prompt
starts on its own. `cleanup` lets ai-core drop stale tool output: it saves tokens but
changes the prompt start.

## Recursion guard
A `send` carrying a reserved `_call_stack` that already contains this agent (a cycle)
or exceeds depth 8 is refused BEFORE the FIFO lock — so an authored agent's AI→AI chain can
neither deadlock nor run away. Don't set `_call_stack` yourself.

## Meta-possibility — any routine orchestrates the whole substrate
Every routine (this AI, a host job, a peer kernel, or any out-of-process caller)
reaches every agent by id. From any of them you can read memory anywhere (`send(<state>,
{read})`), run an inference turn (`send(<ai>, {send, ...})`), and/or spawn compute
(`send(<py>, {start, code})`) — regardless of which kernel owns the target. Code
steps and LLM-call steps are interchangeable, both directions.
