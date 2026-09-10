# nvidia_nim_backend — NVIDIA NIM LLM agent
OpenAI-compatible LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the NIM `Provider`
(HTTPS + Bearer auth + SSE streaming raw text + 429 rate-limit retry)
and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, prompt assembly, ONE-PASS
generation, tool-call parsing + reply routing, context management, and history
all live in `fantastic-ai-core`. **No loop.**

## ONE pass — and the model routes its own output
`send` does exactly ONE provider pass. **No loop**: nothing iterates on the
model's behalf, and emitting no `<call>` ends the turn — stop is the
default and costs nothing.

Tool-calling is RAW and belongs to `fantastic-ai-core`. This backend NEVER sends
the OpenAI `tools`/`tool_choice` fields nor reads `delta.tool_calls`: the model
writes the envelope as TEXT, the NIM `Provider` streams that text through knowing
nothing about it, and ai-core parses and dispatches it.

```
<call name="send">
<target_id>id</target_id>
<reply_to>self</reply_to>
<type>verb</type>
</call>
```

Each call is dispatched and its reply delivered to that call's `reply_to`, which
DEFAULTS TO SELF — leave it out and the answer arrives as the model's next turn,
tagged `[reply to <id>]`; `null`/`"none"` discards it. `reply_to:"self"` is what
makes a chain, one pass at a time, and only because the model asked again. A
caller's optional `send_to` hands the pass's TEXT to one agent id instead, once,
without propagating. A `system_prompt` override replaces the default system block
whole, for a caller that wants a different role.

## Context management — owned HERE
Per-client `chat.json` history, auto-compaction on overflow (`context_strategy`,
default `compact`, + `[context-notice]`), and the `recall` / `context_status`
verbs. The conversation's owner owns its context.

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. The provider connection differs
(HTTPS + Bearer auth + SSE) but verbs and emitted events are
byte-for-byte identical, so any client can swap providers by changing
one `handler_module` field on the agent's record — no client-side
change.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `status` ·
`boot` · `shutdown` · `reflect`.

There is NO `interrupt` verb: an in-flight `send` IS a kernel task. Cancel a
turn with the substrate's universal system verbs — `send "kernel" {type:"ps"}`,
find the row `{agent:<this id>, verb:"send"}`, then `send "kernel"
{type:"cancel", task_id:"<agent>:<n>"}`. The turn closes with
`status(phase="done", reason="cancelled")` + `done`, the SSE stream and the FIFO
lock are released, and the blocked caller's `send` resolves to
`{cancelled:true}`.

Extras beyond the contract:

- THE API KEY COMES FROM THE ENVIRONMENT (`NVIDIA_API_KEY`), read at call time.
  There is no key-storage verb and no sidecar: this bundle never writes the key
  anywhere, so it cannot leak through a record, a reflect or `kernel.list()`.
  An unset variable fails loud NAMING it — a connector stays inert until the
  operator wires it.
- `reflect` reports `has_api_key: bool` only — never the value.

On HTTP 429 BEFORE any chunk has been yielded, the bundle retries
once after sleeping `min(60s, max(1s, Retry-After || 5s))`. A `say`
event + `status(thinking, waiting_on=rate_limit)` event surface the
wait via the caller's inbox. Mid-stream 429 (rare — quota usually
checked up front) propagates as an error.
