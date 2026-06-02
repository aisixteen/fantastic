# ollama_backend — local LLM agent
Talks to a local ollama. Per-client chat threads, FIFO lock, native tool-calls, menu cache. Persistence via `file_agent_id`.

Canonical reference for the **LLM backend contract** every backend
(nvidia_nim, apple_fm, …) speaks, so any caller retargets by changing one
record field — no caller-side change.

## Calling this agent as a workflow unit
`send {type:"send", text, client_id?}` runs ONE inference turn (per-backend FIFO
lock) and streams `token`/`status`/`done` on this agent's OWN inbox, returning
`{response, final}`. It is SHARED COMPUTE reached by id: a scheduler, a host job, a
peer kernel, another AI, or a view all `send` to it by id — threads are kept
per-`client_id`, and any caller that `watch`es this id consumes the same stream.
Optional `system_prompt`/`messages` override the auto-built prompt/history
(stateless mode). Verbs: `send` · `history` · `interrupt` · `status` ·
`refresh_menu` · `reflect` · `boot`.

## Routing is the model's decision
HOW a completion reaches its addressee is the model's: the per-call prompt names
who listens (possibly many); the system prompt carries the `send` signature; the
model `send()`s a named listener or just answers — no `reply_to`. A reserved
`_call_stack` guard refuses cycles / depth>8 before the lock (AI→AI can't deadlock).
