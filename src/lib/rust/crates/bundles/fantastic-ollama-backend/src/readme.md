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

## Bare completion — the tool loop is the CALLER's
This backend does NOT parse tool calls and does NOT loop. `send` performs exactly
ONE provider pass: it streams the model's pure text and returns it. Any tool
convention is the caller's: an authored agent teaches its convention in the
prompt (via `system_prompt`), reads the completion, dispatches, and feeds the
result back as the next `{send, text}` turn. The `OllamaProvider` never receives a
`tools` array and no `message.tool_calls` are read — model-agnostic.

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
and returns `{response, final}`. A first-class peer of `exec_bridge`: a
scheduler, a host job, a peer kernel, or a routine can call it by id. Optional
`system_prompt:str` REPLACES the default system block (the caller supplies its own
role / tool convention / whatever menu it built). There is no caller-supplied history: a one-off completion is a disposable agent with its own store; context for another agent travels in the text, or by grant (`share` → `fork_from`).

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
