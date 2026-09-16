# anthropic_backend — Claude, a context-managed completion agent
Talks to the Anthropic Messages API (default model `claude-opus-4-8`). Key read from `ANTHROPIC_KEY` (or `ANTHROPIC_API_KEY`) in the environment / `.env`. Per-client chat threads, FIFO lock, and automatic context management. **No agentic loop** — one provider pass per `send`. Persistence via `file_bridge_id`.

## Implementation
This bundle is a thin binding over the shared `ai_core` lib. The queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, and all the context machinery + verb bodies live in `ai_core.core`; this module supplies the `AnthropicProvider` builder (endpoint/model from the agent record) and `ai_core.build()` wires the rest into `(VERBS, handler)`.

## Bare completion — the tool loop is the CALLER's
This backend does NOT parse tool calls and does NOT loop. `send` performs exactly ONE
provider pass. It NEVER sends Anthropic `tools`/`input_schema` nor reads `tool_use`
blocks; the `AnthropicProvider` streams plain `text_delta` content, returned verbatim.
Any tool convention is the caller's — an authored agent teaches it in the prompt (via
`system_prompt`), reads the completion, dispatches, and feeds the result back as the
next `{send, text}` turn.

## Context management — owned HERE
Like ollama_backend: per-client `chat.json` history, auto-compaction on overflow
(`context_strategy`, default `compact`, + `[context-notice]`), and the `recall` /
`context_status` verbs. The conversation's owner owns its context.

## Calling this agent
`send {type:'send', text, client_id?}` runs ONE completion (per-backend FIFO lock)
and returns `{response, final}`. A first-class peer of `exec_bridge`: a scheduler,
a host job, a peer kernel, or a routine can call it by id. Optional `system_prompt:str`
REPLACES the default system block (the caller supplies its own role / tool convention).
Optional `messages:list` REPLACES persisted history (fully stateless — no compaction;
the caller owns the context in that mode).

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
