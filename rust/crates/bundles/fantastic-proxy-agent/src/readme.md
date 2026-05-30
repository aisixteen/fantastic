# proxy_agent — host-implemented agents

A bundle that forwards every verb to a host-provided implementation
of [`ProxyAgentHost`]. Hosts are registered per-agent-id, so
multiple `proxy_agent.tools` instances in the same kernel each have
their own behaviour.

Primary use: surfacing host-driven features as first-class agents in
the kernel (`chat_ui`, `settings_ui`, etc.) — addressable, in the
reflect tree, in state events, lifecycle-managed by standard
`create_agent` / `delete_agent`. Same mechanism serves any
host-driven feature: UI views, system-integration bridges, sensor
adapters, clipboard helpers, scripting runtimes, and so on. Naming
is `ProxyAgent` because UI is one consumer, not the only one.

A host is plain Rust: an implementor of the [`ProxyAgentHost`] trait
living in the embedding app (in-process) or a test mock. There is no
foreign-language binding involved.

## How it works

1. The embedding app creates a proxy agent via the standard verb.
   In verb-shape terms it sends `core` a payload like:

   ```json
   {"type":"create_agent","handler_module":"proxy_agent.tools","id":"chat_ui"}
   ```

2. It registers a Rust `ProxyAgentHost` for that id:

   ```rust
   register_host("chat_ui".into(), Arc::new(ChatUiHost::new()));
   ```

3. Other agents address it like any agent — sending to `chat_ui`
   arrives at `host.handle(payload_json)`. The host does whatever
   work it needs and returns a sync reply (an ack for fire-and-
   forget work, a value for queries).

4. The host sends with sender attribution via the kernel's
   `send_json_as` method — state events carry `sender = "chat_ui"`.

5. The host broadcasts state changes via the kernel's `proxy_emit`
   method — watchers of the host agent see them via the standard
   fan-out.

## Verbs (default behaviour)

| verb | no host registered | with host registered |
|---|---|---|
| `reflect` | `{id, sentence: "no host", host_registered: false}` | host's `handle({type:"reflect"})` reply (host_registered overlaid as `true`) |
| `boot` | `{ok: true}` | host's `on_boot()` + `handle({type:"boot"})` |
| `shutdown` | `{ok: true}` | host's `handle({type:"shutdown"})` |
| anything else | `{error, reason: "no_host"}` | host's `handle(payload_json)` |

## Lifecycle

`Bundle::on_delete` fires during cascade — calls `host.on_delete()`
+ drops the host from the registry. No explicit `unregister_host`
call needed if you cascade-delete through the standard verb.

## Threading

`ProxyAgentHost::handle` is sync (`handle(payload_json) -> reply_json`).
A host whose real work is async should kick off a task inside `handle`
and return immediately; the sync return value is just an ack. The
standard streaming-out pattern (the `{queued, stream_id}` reply +
`proxy_emit` for async token feedback) is the canonical way to
surface streamed work back to the kernel.

## Worked example — LLM chat backend

Wrapping an on-device or local LLM is a representative use case for
`proxy_agent`. Same pattern, same verbs, generic substrate.

### Verb surface

The host's `handle(payload_json)` switches on `payload.type`:

| verb | payload | reply |
|---|---|---|
| `send` | `{text, client_id?}` | `{queued: true, stream_id, message_id}` — host kicks streaming task |
| `history` | `{client_id?}` | `{messages, client_id}` |
| `interrupt` | `{client_id?}` | `{interrupted: true}` |
| `backend_state` | `{}` | `{model_available, backend_registered, in_flight, …}` |
| `reflect` | `{}` | identity + provider probes |

This is the same surface `ai_chat_webapp` expects from any chat
backend (`ollama_backend`, `nvidia_nim_backend`, etc.), so dropping
in a proxy_agent-backed chat host with `upstream_id: "fm"` requires
no chat-webapp changes.

### Streaming via `proxy_emit`

`handle({type:"send"})` returns **synchronously** with `{queued,
stream_id}`. The host kicks a background task that:

1. Pulls current tools from the registry by sending the `tools`
   agent `{"type":"list_for_llm"}` and awaiting the reply.
2. Constructs (or reuses) an LLM session configured with those tools.
3. Streams the user message, emitting one event per token plus a
   terminal `done`. In verb-shape terms the host calls the kernel's
   `proxy_emit` with the host's agent id and, per token:

   ```json
   {"type":"token","stream_id":"...","delta":"..."}
   ```

   then, when the turn completes:

   ```json
   {"type":"done","stream_id":"..."}
   ```

Tokens fan out via the kernel's standard watcher mechanism —
anything that watched the host agent (`watch(src:"fm",
watcher:"chat_ui")`) receives them.

### Session lifecycle

If the underlying LLM library is stateful (holds a session/context
object), hold **one session per chat thread** in the host (a stored
field, not a local), bootstrap it on the first `send`, and reuse it
for every subsequent turn. This keeps context coherent across turns
and avoids re-initialising the model on every message:

- On the first turn the host can replay `history_json` from the
  kernel (if any) to bootstrap the session.
- On subsequent turns the session owns the conversation context and
  the host can ignore the kernel's history field.

The kernel's history stays authoritative for **telemetry + replay**;
the session's transcript stays authoritative for **what the LLM saw**.

### Why this lives on proxy_agent

There's no LLM-specific bundle. The proxy_agent substrate handles:

- Host registration per agent_id
- Sync verb dispatch
- Cascade-delete hooks (host can dispose its session via `on_delete`)
- Sender-attributed inbound (`send_json_as`) and async outbound
  (`proxy_emit`)

Anything LLM-specific (the verb table, the session lifecycle, the
history shape, the availability probes) is **the host's job**. The
kernel doesn't know it's talking to an LLM.

See `fantastic-tools/src/readme.md` for the companion piece: how the
chat backend pulls registered tools from the `tools.tools` agent on
every `send`.
