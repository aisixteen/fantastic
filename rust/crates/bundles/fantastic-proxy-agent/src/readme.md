# proxy_agent — host-implemented agents

A bundle that forwards every verb to a host-provided implementation
of [`ProxyAgentHost`]. Hosts are registered per-agent-id, so
multiple `proxy_agent.tools` instances in the same kernel each have
their own behaviour.

Primary use: SwiftUI views as first-class agents in the kernel
(`chat_ui`, `settings_ui`, etc.) — addressable, in the reflect
tree, in state events, lifecycle-managed by standard
`create_agent` / `delete_agent`. Same mechanism serves any
host-driven feature: AppIntents bridges, Vision adapters, Clipboard
helpers, JavaScript runtimes, and so on. Naming is `ProxyAgent`
because UI is one consumer, not the only one.

## How it works

1. The Swift app creates a proxy agent via the standard verb:

   ```swift
   try await kernel.sendJson("core", #"""
     {"type":"create_agent","handler_module":"proxy_agent.tools","id":"chat_ui"}
   """#)
   ```

2. It registers a Swift `ProxyAgent` callback for that id:

   ```swift
   try kernel.registerProxyAgent(agentId: "chat_ui", host: ChatUIHost())
   ```

3. Other agents address it like any agent — `kernel.send("chat_ui",
   payload)` arrives at `host.handle(payload_json)`. Swift re-
   dispatches to `MainActor` for SwiftUI mutations and returns a
   sync ack.

4. The UI sends with sender attribution via the new
   `Kernel::send_json_as` method — state events carry
   `sender = "chat_ui"`.

5. The UI broadcasts state changes via `Kernel::proxy_emit` —
   watchers of the UI agent see them via the standard fan-out.

## Verbs (default behaviour)

| verb | no host registered | with host registered |
|---|---|---|
| `reflect` | `{id, sentence: "no host", host_registered: false}` | host's `handle({type:"reflect"})` reply (host_registered overlaid as `true`) |
| `boot` | `{ok: true}` | host's `on_boot()` + `handle({type:"boot"})` |
| `shutdown` | `{ok: true}` | host's `handle({type:"shutdown"})` |
| anything else | `{error, reason: "no_host"}` | host's `handle(payload_json)` |

## Lifecycle

`Bundle::on_delete` fires during cascade — calls `host.on_delete()`
+ drops the host from the registry. No explicit `unregisterProxyAgent`
needed if you cascade-delete through the standard verb.

## Threading

UniFFI 0.29 callback methods are sync. Swift impls re-dispatch to
`MainActor` via `Task { @MainActor in … }`; the sync return value
is just an ack. The standard streaming-out pattern (the `{queued,
stream_id}` reply + `proxy_emit` for async token feedback) is the
canonical way to surface streamed work back to the kernel.

## Worked example — LLM chat backend

Wrapping an on-device LLM (Apple Foundation Models, future local
models, etc.) is the **primary use case** for `proxy_agent` after
the standalone FM bundle was retired. Same pattern, same verbs,
generic substrate.

### Verb surface

The Swift host's `handle(payloadJson)` switches on `payload.type`:

| verb | payload | reply |
|---|---|---|
| `send` | `{text, client_id?}` | `{queued: true, stream_id, message_id}` — host kicks streaming task |
| `history` | `{client_id?}` | `{messages, client_id}` |
| `interrupt` | `{client_id?}` | `{interrupted: true}` |
| `backend_state` | `{}` | `{apple_intelligence_available, model_available, backend_registered, in_flight, …}` |
| `reflect` | `{}` | identity + provider probes |

This is the same surface `ai_chat_webapp` expects from any chat
backend (`ollama_backend`, `nvidia_nim_backend`, etc.), so dropping
in a Swift-backed proxy_agent with `upstream_id: "fm"` requires no
chat-webapp changes.

### Streaming via `proxy_emit`

`handle({type:"send"})` returns **synchronously** with `{queued,
stream_id}`. The host kicks a `Task { @MainActor in … }` that:

1. Pulls current tools from the registry:
   ```swift
   let toolsJson = try await kernel.sendJson(
       targetId: "tools",
       payloadJson: #"{"type":"list_for_llm"}"#)
   ```
2. Constructs (or reuses) a `LanguageModelSession` with the tools.
3. Streams the user message:
   ```swift
   for try await token in session.streamResponse(to: userText) {
     try await kernel.proxyEmit(
         agentId: "fm",
         eventJson: #"{"type":"token","stream_id":"...","delta":"..."}"#)
   }
   try await kernel.proxyEmit(agentId: "fm",
       eventJson: #"{"type":"done","stream_id":"..."}"#)
   ```

Tokens fan out via the kernel's standard watcher mechanism —
anything that did `kernel.watch(src:"fm", watcher:"chat_ui")`
receives them.

### Session lifecycle (Apple FM caveat)

Apple's `LanguageModelSession` has a known iOS 26.x issue: creating
a **second** session whose LLM invokes a tool crashes inside Apple's
framework (`SwiftUI/AppGraph.swift:26 — AppGraph.shared may only be
set once!`). Workaround: **hold one session per chat thread** in
the host (stored property, not local variable), bootstrap it on
first `send`, reuse for every subsequent turn:

```swift
final class FoundationModelsProxyHost: ProxyAgent {
  private var session: LanguageModelSession?  // ← persistent

  func handle(payloadJson: String) -> String {
    // ...
    if session == nil {
      session = LanguageModelSession(instructions: ..., tools: ...)
    }
    // reuse session for every turn
  }
}
```

Apple's session keeps its own transcript internally; on the first
turn the host can replay `historyJson` from the kernel (if any) to
bootstrap; on subsequent turns the session owns context and the
host can ignore the kernel's history field. The kernel's history
stays authoritative for **telemetry + replay**, the session's
transcript stays authoritative for **what the LLM saw**.

### Why this lives on proxy_agent

There's no Apple-FM-specific bundle anymore. The proxy_agent
substrate handles:
- Host registration per agent_id
- Sync verb dispatch
- Cascade-delete hooks (host can dispose session via `on_delete`)
- Sender-attributed inbound (`send_json_as`) and async outbound
  (`proxy_emit`)

Anything FM-specific (the verb table, the session lifecycle, the
history shape, the availability probes) is **the Swift host's
job**. The kernel doesn't know it's talking to an LLM.

See `fantastic-tools/src/readme.md` (Swift Integration) for the
companion piece: how the chat backend pulls registered tools from
the `tools.tools` agent on every `send`.
