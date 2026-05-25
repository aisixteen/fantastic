# tools — registrable tool calling for LLM-using agents

A registry of tool definitions that LLM-using bundles (FM, ollama,
nvidia, …) prepend to every model call. The registry maps
`tool_name → {agent_id, verb, description, parameters_schema, sender}`.
**Dispatch goes through `kernel.send`** — every tool is an existing
agent in the kernel; this bundle is just the naming + schema layer
over the kernel's existing routing.

The primary use is wiring native features (filesystem, vision,
clipboard, app intents, calculators, search) into Apple Foundation
Models / OpenAI / Anthropic tool-calling. Same mechanism serves any
LLM backend — the bundle is provider-agnostic; FM happens to be the
first integration.

## Mental model

1. **`kernel.send(target_id, payload)` IS the tool-call primitive.**
   Every agent in the kernel can answer verbs and return JSON. A
   "tool" is just an agent with a name + description + JSON Schema
   for its args.
2. **The registry is a naming + schema layer.** Lookup is by tool
   name (what the LLM sees). Every entry stores its dispatch
   coordinates (`agent_id` + optional `verb`) and the sender that
   registered it (cleanup key).
3. **State controlled outside, immutable unless unset, always
   prependable.** The embedding app owns the registry: it
   re-registers tools at every kernel boot. Once registered an
   entry stays until explicitly unregistered. Every LLM call
   automatically gets the current set — no per-call opt-in.

## Verb reference

| verb | payload | reply |
|---|---|---|
| `reflect` | `{}` | `{id, sentence, kind: "tools", tool_count, verbs, emits}` |
| `register` | `{name, agent_id, verb?, description, parameters_schema}` | `{ok: true, name}` or `{error, reason}` |
| `unregister` | `{name}` | `{ok: true, name}` or `{error, reason: "not_found"}` |
| `unregister_by_sender` | `{}` (sender from `with_sender`) | `{ok: true, removed: <n>, sender}` |
| `clear` | `{}` | `{ok: true, removed: <n>}` |
| `list` | `{}` | `{tools: [full entries], count}` — debug / inspection |
| `list_for_llm` | `{}` | `{tools: [{name, description, parameters}]}` — LLM-facing shape |
| `dispatch` | `{name, arguments}` | reply from `kernel.send(entry.agent_id, {type: verb_or_name, ...arguments})` |

### Sender attribution

`register` and `unregister_by_sender` take an explicit `sender` field
in the payload. The kernel's own `send` rewraps the `with_sender`
task-local with the target id before invoking a bundle — bundles
can't read the original sender from `current_sender()` — so we make
it explicit in the payload instead.

The UniFFI sugar wrappers (`kernel.registerTool`,
`kernel.unregisterToolsBySender`, …) take a `sender_id` param and
inject it into the JSON payload transparently. Raw-send callers
include `"sender": "..."` in the payload themselves.

`unregister_by_sender` drops everything matching the given sender —
useful for logout, mode change, agent teardown — without needing the
caller to remember every tool name.

## Register — minimum

The smallest viable registration: name, dispatch target, description,
schema (even if no args). `sender` defaults to `"anonymous"` if omitted.

```json
{
  "type": "register",
  "name": "ping",
  "agent_id": "health_check",
  "description": "Returns pong. Use to verify backend liveness.",
  "parameters_schema": {
    "type": "object",
    "properties": {},
    "additionalProperties": false
  }
}
```

What gets recorded:

```text
ToolEntry {
  name: "ping",
  agent_id: AgentId("health_check"),
  verb: None,                      // omitted → dispatch as {"type":"ping", ...}
  description: "Returns pong. ...",
  parameters_schema: { "type":"object", ... },
  sender: AgentId("anonymous"),    // no sender field → default
}
```

## Register — maximum

Every field exercised: explicit verb, sender attribution, rich
schema with enums, constraints, conditionals, required fields,
defaults.

```json
{
  "type": "register",
  "name": "search_documents",
  "agent_id": "doc_index",
  "verb": "search",
  "sender": "docs_owner",
  "description": "Search the company document index. Returns up to N matching documents with snippets. Prefer this over web search for internal / policy questions.",
  "parameters_schema": {
    "type": "object",
    "properties": {
      "query":            { "type": "string", "minLength": 1,
                            "description": "Natural-language search query." },
      "limit":            { "type": "integer", "minimum": 1, "maximum": 50, "default": 5,
                            "description": "Maximum number of results." },
      "scope":            { "type": "string", "enum": ["all","engineering","policy","support","hr"],
                            "default": "all",
                            "description": "Which doc collection to search." },
      "since":            { "type": "string", "format": "date",
                            "description": "Only return documents modified on or after this date (YYYY-MM-DD)." },
      "include_archived": { "type": "boolean", "default": false },
      "ranking":          { "type": "string", "enum": ["relevance","recency"], "default": "relevance" }
    },
    "required": ["query"],
    "additionalProperties": false,
    "if":   { "properties": { "ranking": { "const": "recency" } } },
    "then": { "required": ["since"] }
  }
}
```

When the LLM emits `search_documents(query="...", limit=10, scope="policy")`,
the bundle does:

```text
kernel.send(
  AgentId::from("doc_index"),
  {"type":"search","query":"...","limit":10,"scope":"policy"}
)
```

## Unregister

**Surgical (by name):**

```json
{"type": "unregister", "name": "search_documents"}
```

**Mass cleanup (drop everything for a given sender):**

```json
{"type": "unregister_by_sender", "sender": "docs_owner"}
```

The sugar wrapper hides this: `kernel.unregisterToolsBySender(senderId:
"docs_owner")` injects the `sender` field for you.

**Nuke (admin / tests):**

```json
{"type": "clear"}
```

## Verb defaults

When an entry's `verb` is `None`, dispatch sends
`{"type": <tool_name>, ...arguments}`. So a tool named `"ping"` with
no explicit verb lands as `{"type":"ping", ...args}` at its dispatch
target. Set `verb` explicitly when the tool's name and the dispatch
target's verb differ (e.g. a tool named `search_documents` that
dispatches verb `search` on agent `doc_index`).

## Swift integration

### 1. Boot the tools agent at kernel startup

```swift
_ = try await kernel.sendJson(
    targetId: "core",
    payloadJson: #"{"type":"create_agent","handler_module":"tools.tools","id":"tools"}"#
)
```

Do this once at app launch, after kernel bootstrap.

### 2. Register tools (typed sugar OR raw send)

The fantastic-uniffi bridge exposes typed convenience wrappers:

```swift
_ = try await kernel.registerTool(
    senderId: "weather_provider",         // ← cleanup key
    name: "get_weather",
    agentId: "weather_provider",          // ← dispatch target
    verb: nil,                            // ← uses tool name as verb
    description: "Returns the current weather for a city.",
    parametersSchemaJson: #"""
        {
          "type": "object",
          "properties": {
            "city":  { "type": "string", "minLength": 1 },
            "units": { "type": "string", "enum": ["celsius","fahrenheit"], "default": "celsius" }
          },
          "required": ["city"],
          "additionalProperties": false
        }
    """#
)
```

Equivalent raw send (no sugar wrapper, include `sender` in the
payload):

```swift
_ = try await kernel.sendJson(
    targetId: "tools",
    payloadJson: #"""
        {
          "type": "register",
          "name": "get_weather",
          "agent_id": "weather_provider",
          "sender": "weather_provider",
          "description": "...",
          "parameters_schema": { ... }
        }
    """#
)
```

### 3. Pull tools from the chat backend (a `proxy_agent` host)

The chat backend is a `proxy_agent.tools` agent with a Swift host
that implements `ProxyAgentHost.handle`. On `{type:"send"}`, the
host pulls the current tool set from the kernel and feeds it to its
`LanguageModelSession`:

```swift
final class FoundationModelsProxyHost: ProxyAgent {
  private var session: LanguageModelSession?
  private let kernel: Kernel

  func handle(payloadJson: String) -> String {
    let payload = try? JSONSerialization.jsonObject(with: payloadJson.data(using:.utf8)!)
    let dict = payload as? [String:Any] ?? [:]
    switch dict["type"] as? String {
    case "send":
      let stream_id = nextStreamId()
      Task { @MainActor in
        // Pull current tools BEFORE constructing the LMSession.
        let toolsJson = try await kernel.sendJson(
            targetId: "tools",
            payloadJson: #"{"type":"list_for_llm"}"#)
        let tools = parseTools(toolsJson, kernel: kernel)   // → [LMTool]
        if session == nil {
          session = LanguageModelSession(
              instructions: systemPrompt,
              tools: tools)
        }
        // Stream the user message via the held session;
        // push tokens out via kernel.proxyEmit on this agent's inbox.
        for try await token in session!.streamResponse(to: userText) {
          try await kernel.proxyEmit(
              agentId: "fm",
              eventJson: #"{"type":"token","stream_id":"\#(stream_id)","delta":"\#(token)"}"#)
        }
        try await kernel.proxyEmit(agentId: "fm",
            eventJson: #"{"type":"done","stream_id":"\#(stream_id)"}"#)
      }
      return #"{"queued":true,"stream_id":"\#(stream_id)"}"#
    case "history":     return historyJson()
    case "interrupt":   return interruptCurrent()
    case "backend_state": return backendStateJson()
    case "reflect":     return reflectJson()
    default:            return #"{"ok":true}"#
    }
  }
}

// At kernel boot:
_ = try await kernel.sendJson(targetId:"core",
    payloadJson: #"{"type":"create_agent","handler_module":"proxy_agent.tools","id":"fm"}"#)
kernel.registerProxyAgent(agentId: "fm",
    host: FoundationModelsProxyHost(kernel: kernel))
```

Tokens flow as **emit events on the fm agent's own inbox** —
`ai_chat_webapp` (or any other consumer) watches the `fm` agent via
the kernel's standard watcher fanout and receives them.

### 4. Build a `LanguageModelSession.Tool` from each entry

iOS / macOS 26+ ships `DynamicGenerationSchema(name:, schema:)` — pass
the tool's `parameters` (JSON Schema) directly:

```swift
struct KernelTool: Tool {
  let name: String
  let description: String
  let parameters: GenerationSchema   // built from parameters_schema

  func call(arguments: ToolArguments) async throws -> ToolOutput {
    // Bridge LLM args → kernel.dispatch_tool → reply
    let argsJson = try arguments.jsonString
    let replyJson = try await kernel.dispatchTool(
        name: self.name,
        argumentsJson: argsJson
    )
    return .json(replyJson)
  }
}
```

Each tool's `call(...)` closure dispatches back through the kernel
via `kernel.dispatchTool(name:argumentsJson:)`. The reply (whatever
the dispatch target returns from `kernel.send`) feeds back to the
session as the tool's output. The session then continues generating
until it produces text or another tool call.

### 5. Cleanup on logout / mode change

```swift
_ = try await kernel.unregisterToolsBySender(senderId: "weather_provider")
```

Drops every tool registered with `senderId: "weather_provider"`. No
need to remember every tool name.

## Conditionality patterns

Three layers, each at a different scope. Pick the right layer for
each kind of condition.

### Per-arg (inside one tool's schema)

JSON Schema's `if`/`then`/`else`, `oneOf`, `dependentRequired`,
`dependentSchemas` all work — FM's guided generation honors them
with slight adherence drop on complex shapes.

```json
{
  "type": "object",
  "properties": {
    "mode":     { "enum": ["quick","detailed"] },
    "language": { "type": "string" }
  },
  "if":   { "properties": { "mode": { "const": "detailed" } } },
  "then": { "required": ["language"] }
}
```

Use for: argument-internal rules ("if A=x then B is required",
"exactly one of B/C").

### Per-tool (which tools exist in the registry right now)

**The registry itself is the conditional gate.** A tool that isn't
registered cannot be called — the LLM literally doesn't see it.

```swift
// Login → these tools become available
_ = try await kernel.registerTool(senderId: "auth", name: "get_user_profile", ...)
_ = try await kernel.registerTool(senderId: "auth", name: "post_message", ...)

// Logout → drop them all
_ = try await kernel.unregisterToolsBySender(senderId: "auth")
```

Use for: state-gated capabilities (logged-in tools, mode-specific
tools, feature-flagged tools, "only when network is up").

### Runtime (preconditions at call time)

Some conditions can't be hoisted to the schema or the registry — for
example "must have called `create_session` before `send_message`".
Validate at dispatch time and return a structured error from the
dispatch target:

```rust
if !session_exists() {
    return json!({
        "error": "no active session",
        "reason": "precondition_failed",
        "needs": ["create_session"],
    });
}
```

The LLM reads the error in the next turn and corrects. Standard
tool-error feedback loop.

## Dispatch flow

```text
       ┌───────────────────────────┐
       │  LLM emits tool_call:     │
       │  name="get_weather"       │
       │  args={"city":"Paris"}    │
       └─────────────┬─────────────┘
                     │
                     ▼
       ┌───────────────────────────┐
       │ Swift Tool.call(args)     │
       │  ↳ kernel.dispatchTool(   │
       │      name, argsJson)      │
       └─────────────┬─────────────┘
                     │  UniFFI
                     ▼
       ┌───────────────────────────┐
       │ tools.tools::dispatch     │
       │  lookup name → entry      │
       │  build {type: verb,       │
       │         ...args}          │
       └─────────────┬─────────────┘
                     │  kernel.send(entry.agent_id, ...)
                     ▼
       ┌───────────────────────────┐
       │ Dispatch target agent     │
       │ (could be a proxy_agent   │
       │  backed by Swift, a Rust  │
       │  bundle, or anything)     │
       └─────────────┬─────────────┘
                     │  reply (JSON Value)
                     ▼
            back through dispatchTool → Tool.call return →
            LanguageModelSession continues → tokens stream → done
```

## Failure modes

| condition | reply shape |
|---|---|
| tool name missing from registry | `{error, reason: "tool_not_found"}` |
| `dispatch` payload missing `name` | `{error, reason: "invalid_args"}` |
| `dispatch` `arguments` not an object | `{error, reason: "invalid_args"}` |
| `register` missing required field | `{error, reason: "invalid_args"}` |
| `unregister` for unknown name | `{error, reason: "not_found"}` |
| `unregister_by_sender` with no `sender` field | `{error, reason: "invalid_args"}` |
| dispatch target agent doesn't exist | `{error: "no agent <id>"}` from `kernel.send` |
| dispatch target's bundle returns error | passes through unchanged — LLM sees the error and retries |

## Selftest

See `selftest.md` for the CLI-driven verb wire-shape verification
(register min / max / dispatch / unregister / dispatch-after-removal /
unregister_by_sender). The cargo unit tests + runnable example
cover the in-process round-trip end to end.
