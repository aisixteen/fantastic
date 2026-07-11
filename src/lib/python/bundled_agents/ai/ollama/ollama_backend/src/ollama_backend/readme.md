# ollama_backend â local LLM, a context-managed completion agent
Talks to a local ollama. Per-client chat threads, FIFO lock, and automatic
context management (compaction / recall / context_status). Persistence via
`file_bridge_id`. **No agentic loop** â one provider pass per `send`.

## Implementation
A thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt
assembly, the ONE-PASS `_run`, and all the context machinery live in
`ai_core.core`; this module supplies the `OllamaProvider` builder (endpoint /
model from the agent record) and `ai_core.build()` wires the rest into
`(VERBS, handler)`.

## Bare completion â the tool loop is the CALLER's
This backend does NOT parse tool calls and does NOT loop. `send` performs exactly
ONE provider pass: it streams the model's pure text and returns it. Any tool
convention is the caller's: an authored agent teaches its convention in the
prompt (via `system_prompt`), reads the completion, dispatches, and feeds the
result back as the next `{send, text}` turn. The `OllamaProvider` never receives a
`tools` array and no `message.tool_calls` are read â model-agnostic.

## Context management â owned HERE (the conversation's owner owns its context)
Because this backend owns the per-client conversation, it owns the context:
- **history** â every `send` loads + appends the per-client `chat.json` thread;
- **auto-compaction** â when the assembled prompt overflows the model's window,
  the configured `context_strategy` (default `compact`) summarizes / elides older
  turns ONCE at turn entry and inserts a `[context-notice]`; the durable
  transcript is never trimmed;
- **`recall {query?, limit?}`** â page dropped turns back from the durable store;
- **`context_status`** â inspect budget + last compaction + the model's reaction.

## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `python_runtime`: a
scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
`system_prompt:str` REPLACES the default system block (the caller supplies its own
role / tool convention / whatever menu it built). Optional `messages:list`
REPLACES persisted history (fully stateless â no `file_bridge_id` needed, and no
compaction â the caller owns the context in that mode).

## Recursion guard
A `send` carrying a reserved `_call_stack` that already contains this agent (a
cycle) or exceeds depth 8 is refused BEFORE the FIFO lock â so an authored agent's AI→AI
chain can neither deadlock nor run away. Don't set `_call_stack` yourself.

## Meta-possibility â any routine orchestrates the whole substrate
Every routine (an authored agent, a host job, a peer kernel, any out-of-process caller)
reaches every agent by id. From any of them you can read memory anywhere
(`send(<state>, {read})`), run an inference turn (`send(<ai>, {send, ...})`),
and/or spawn compute (`send(<py>, {start, code})`) â regardless of which kernel
owns the target. Code steps and LLM-call steps are interchangeable, both
directions.
