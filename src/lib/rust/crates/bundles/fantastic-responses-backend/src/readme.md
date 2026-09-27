# responses_backend — an OpenAI Responses-API, context-managed completion agent
Talks to any endpoint that serves the OpenAI Responses API (`POST …/v1/responses`): OpenAI itself, Ollama ≥ 0.13.4, or a proxy in front of an open-weights model. `endpoint` (the full `…/v1/responses` URL) and `model` are REQUIRED on the record — nothing is guessed; the key is read from `OPENAI_API_KEY` in the environment / `.env`. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Conversation in its memory store (`memory_store_id`).

## Implementation
This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `ResponsesProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

## The wire
Stateless: every pass sends `store:false` and the whole conversation as `input` items (the system block as `instructions`) — the conversation is this agent's, never the endpoint's. The stream is read on BOTH channels: `response.reasoning_text` / `reasoning_summary_text` deltas are wrapped in `<think>…</think>` (parsed for calls, stripped before the turn is stored), `response.output_text` deltas are the output. A turn ends `completed`; one cut at `max_output_tokens` (`response.incomplete`) is a loud error on a main turn and an answer (`last_stop_reason:"max_tokens"`) on a bounded side call such as a fold summary. Sampling knobs on the record (`sampling`) merge verbatim into the request body.

## Bare completion — the tool loop is the CALLER's
This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
provider pass. It NEVER sends Responses `tools` nor reads `function_call` items (one
arriving is a loud error); the stream's text is returned verbatim. Any tool convention
is the caller's — an authored agent teaches it in the prompt (via `system_prompt`),
reads the completion, dispatches, and feeds the result back as the next `{send, text}`
turn.

## Context management — owned HERE
The conversation lives in this agent's MEMORY STORE — `memory_store_id` on the record
is REQUIRED (a `send` fails loud naming it): every turn, fold and view is a row under
this agent's key. Reply BODIES are never stored — a reply row keeps its one-line
`origin`, and after a restart the model re-reads from it. Compaction fires at 0.8 of
the budget (calibrated by the endpoint's reported prompt size when it reports one)
with the per-agent `context_strategy` (default `compact`; `fold`, `truncate`) and a
`[context-notice]`; `context_status`, `context_recall`, `history {fold?}`, `grep` and
`context_clear` (a boundary, nothing deleted) inspect and page it. Sharing is by
GRANT, by the owner's own choice: it `share`s a scope of its rows through the store;
another agent starts from it with `fork_from {owner, grant, message}` and asks for a
third agent's row with `request_share`. `watch {body?}` / `unwatch` subscribe to this
agent's row events (a digest; bodies only for a watcher the rows are shared with).
## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
REPLACES the default system block (the caller supplies its own role / tool convention).
There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text, or by grant (`share` → `fork_from`).

## Streaming
You stream `token`/`status`/`done` on your OWN id. Any caller that `watch`es your id
consumes the same stream (the driving caller, a browser tab, a peer). You do not route
your own output — you complete; the caller reads it and decides what to do next.

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
