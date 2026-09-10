# anthropic_backend — Anthropic (Claude) LLM agent
Anthropic Messages-API LLM backend. Thin Provider + Bundle over
`fantastic-ai-core`: this crate supplies only the Anthropic `Provider`
(HTTPS + `x-api-key` auth + event-typed SSE streaming raw text + 429
rate-limit retry) and a thin `Bundle` that dispatches every verb
through `fantastic-ai-core`. The FIFO lock, prompt assembly, ONE-PASS
generation, tool-call parsing + reply routing, context management, and history
all live in `fantastic-ai-core`. **No loop.**

This crate implements the **LLM backend contract** defined in
`fantastic-ai-core`'s module header. Verbs + events are identical across
backends, so a caller retargets by changing one id.

## ONE pass — and the model routes its own output
`send` does exactly ONE provider pass. **No loop**: nothing iterates on the
model's behalf, and emitting no `<call>` ends the turn — stop is the
default and costs nothing.

Tool-calling is RAW and belongs to `fantastic-ai-core`. By default this backend
sends no Anthropic `tools`/`input_schema`: the model writes the envelope as TEXT,
the Anthropic `Provider` streams that text through as plain `text_delta` content
knowing nothing about it, and ai-core parses and dispatches it. Wire translation
is just the `system` role → Anthropic's top-level `system` param + alternating
user/assistant TEXT turns.

**No `tool_use` route.** It existed, declared one `send` tool and translated the
blocks back into a text envelope; it is DELETED. The envelope it translated into
no longer exists, and a model writing JSON tool calls is writing a grammar this
kernel does not parse — so a native `tool_use` block is a LOUD ERROR now. Some
`/cc`-style routes emit one for a request carrying no `tools` key at all, which
is exactly the case worth failing on: its arguments are JSON nobody reads, and a
silent drop ends the turn looking finished with the call lost.

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
Per-client `chat.json` history, auto-compaction on overflow, and the `recall` /
`context_status` verbs. The conversation's owner owns its context.

**Streaming routing**: this backend uses per-client-inbox routing
(`CallerRoute::PerClientInbox`). Every stream event is emitted on the
CALLER's own `client_id` inbox — NOT the backend agent's own inbox.
A watcher of the backend agent's id does NOT see these events.

Verbs: `send` · `history` · `recall` · `context_status` ·
`status` · `boot` · `shutdown` ·
`reflect`.

There is NO `interrupt` verb: an in-flight `send` IS a kernel task. Cancel a
turn with the substrate's universal system verbs — `send "kernel" {type:"ps"}`,
find the row `{agent:<this id>, verb:"send"}`, then `send "kernel"
{type:"cancel", task_id:"<agent>:<n>"}`. The turn closes with
`status(phase="done", reason="cancelled")` + `done`, the HTTP stream and the
FIFO lock are released, and the blocked caller's `send` resolves to
`{cancelled:true}`.

Extras beyond the contract:

- THE API KEY COMES FROM THE ENVIRONMENT (`ANTHROPIC_KEY`, else `ANTHROPIC_API_KEY`), read at call time.
  There is no key-storage verb and no sidecar: this bundle never writes the key
  anywhere, so it cannot leak through a record, a reflect or `kernel.list()`.
  An unset variable fails loud NAMING it — a connector stays inert until the
  operator wires it.
- `reflect` reports `has_api_key: bool` only — never the value.

On HTTP 429 BEFORE any chunk has been yielded, the bundle retries
once after sleeping `min(60s, max(1s, Retry-After || 5s))`. A `say`
event + `status(thinking, waiting_on=rate_limit)` event surface the
wait via the caller's inbox. Mid-stream 429 propagates as an error.
