# ollama_backend — local LLM, a context-managed completion agent
Talks to a local ollama. Per-client chat threads, FIFO lock, and automatic
context management (compaction / recall / context_status). Conversation in its
memory store (`memory_store_id`). **No agentic loop** — one provider pass per `send`.

## Implementation
A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
assembly, the ONE-PASS `_run`, and all the context machinery live in
`ai_core.core`; this module supplies the `OllamaProvider` builder (endpoint /
model from the agent record) and `ai_core.build()` wires the rest into
`(VERBS, handler)`.

## The record
`endpoint` (the server URL, e.g. `http://127.0.0.1:11434`) is required. `model` left
out is READ FROM THE SERVER before a verb that runs a turn (`send`, `inquiry`,
`context_recall`) — the ONE model `GET {endpoint}/api/tags` lists — and written back
to the record; a server listing several names them and the record must pick one. A
server that does not answer refuses the verb (`bad_arg`, `arg: model`). Nothing about
the window is read: ollama loads a model with the request's `num_ctx`, else the
Modelfile's, else its own default, which the API does not expose — set `num_ctx` (or
`context_window`). `credentials` (`{store, ref}`) names a key for a server behind a
door: read before each request, sent as `Authorization: Bearer`; absent, no header.

## Bare completion + the `<call>` grammar — no native tool API
`send` performs exactly ONE provider pass: the `OllamaProvider` streams the model's text and never receives a `tools` array nor reads `message.tool_calls`. Tool-calling is RAW and shared by every backend: `ai_core` parses the model's `<call name=…>` tags from the streamed text, dispatches them as ONE batch (`kernel.send([...])`), and each destination's results arrive as ONE fresh turn — there is no loop inside a pass.

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
and returns `{response, final}`. A first-class peer of `exec_bridge`: a
scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
`system_prompt:str` REPLACES the default system block (the caller supplies its own
role / tool convention / whatever menu it built).
The record's own `system_prompt` (a non-empty string; `null` clears it) replaces
the default block on EVERY turn of this agent — a main turn, a nudge, a turn a reply
wakes — read live each turn; a per-call `system_prompt` beats it for that delivery.
There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text.

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

## Recursion guard
A `send` carrying a reserved `_call_stack` that already contains this agent (a
cycle) or exceeds depth 8 is refused BEFORE the FIFO lock â so an authored agent's AI→AI
chain can neither deadlock nor run away. Don't set `_call_stack` yourself.

## Meta-possibility — any routine orchestrates the whole substrate
Every routine (an authored agent, a host job, a peer kernel, any out-of-process caller)
reaches every agent by id. From any of them you can read memory anywhere
(`send(<state>, {read})`), run an inference turn (`send(<ai>, {send, ...})`),
and/or spawn compute (`send(<py>, {start, code})`) — regardless of which kernel
owns the target. Code steps and LLM-call steps are interchangeable, both
directions.
