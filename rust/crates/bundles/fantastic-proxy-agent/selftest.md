# proxy_agent selftest

> scopes: bundle wire-shape, graceful-degrade
> requires: `cargo build --release --bin fantastic`
> out-of-scope: bidirectional streaming (no Swift host in the CLI)

CLI-driven smoke for the verb wire shape. The streaming +
bidirectional path is covered by the cargo unit tests and the
runnable example (`cargo run -p fantastic-proxy-agent --example
proxy_mock_session`), since the CLI has no Swift host wired in.

## Pre-flight

```bash
rm -rf /tmp/fk_proxy && mkdir -p /tmp/fk_proxy && cd /tmp/fk_proxy
FANTASTIC=/path/to/rust/target/release/fantastic
```

## Tests

### Test 1: cold-boot creates a proxy_agent

```bash
$FANTASTIC reflect >/dev/null
$FANTASTIC core create_agent handler_module=proxy_agent.tools id=chat_ui \
  > /tmp/fk_proxy/create.json
jq -e '.id == "chat_ui"' /tmp/fk_proxy/create.json
test -d .fantastic/agents/chat_ui
jq -e '.handler_module == "proxy_agent.tools"' .fantastic/agents/chat_ui/agent.json
```

Expect: standard create_agent flow + record persisted (merge-only
adapter from step 1).

### Test 2: reflect with no host carries host_registered=false

```bash
$FANTASTIC chat_ui reflect > /tmp/fk_proxy/reflect.json
jq -e '.host_registered == false' /tmp/fk_proxy/reflect.json
jq -e '.kind == "proxy_agent"' /tmp/fk_proxy/reflect.json
jq -e '.verbs["*"] | type == "string"' /tmp/fk_proxy/reflect.json
jq -e '.sentence | contains("no host registered")' /tmp/fk_proxy/reflect.json
```

Expect: bundle self-describes when no host is registered.

### Test 3: boot without host returns ok

```bash
$FANTASTIC chat_ui boot > /tmp/fk_proxy/boot.json
jq -e '.ok == true' /tmp/fk_proxy/boot.json
jq -e '.host_registered == false' /tmp/fk_proxy/boot.json
```

### Test 4: arbitrary verb degrades gracefully

```bash
$FANTASTIC chat_ui render_token delta=hi > /tmp/fk_proxy/render.json
jq -e '.reason == "no_host"' /tmp/fk_proxy/render.json
jq -e '.error | contains("no host registered")' /tmp/fk_proxy/render.json
```

Expect: structured error — Swift / UI client uses `reason` to
decide UX (e.g. show "loading" until host is registered).

### Test 5: shutdown without host returns ok

```bash
$FANTASTIC chat_ui shutdown > /tmp/fk_proxy/shut.json
jq -e '.ok == true' /tmp/fk_proxy/shut.json
```

### Test 6: cascade delete

```bash
$FANTASTIC core create_agent handler_module=proxy_agent.tools id=settings_ui >/dev/null
$FANTASTIC core delete_agent id=settings_ui > /tmp/fk_proxy/del.json
jq -e '.deleted == true' /tmp/fk_proxy/del.json
test ! -e .fantastic/agents/settings_ui
```

Expect: standard cascade. Bundle's `on_delete` hook runs (no host
to call, so it just clears the registry — no observable effect at
the CLI level).

### Test 7: chat-backend verb shape via CLI (no host)

Verifies the wire shape the Swift LLM chat backend speaks. CLI
process has no Swift host registered, so every verb returns the
graceful `no_host` envelope — that's exactly what proves the
verbs are reaching the bundle. Multi-step state verification
happens via cargo unit tests + the runnable example (the CLI
can't drive a Swift host).

```bash
$FANTASTIC core create_agent handler_module=proxy_agent.tools id=fm \
  > /tmp/fk_proxy/fm_create.json
jq -e '.id == "fm"' /tmp/fk_proxy/fm_create.json

# send → no_host envelope (proves the verb reached the bundle)
$FANTASTIC fm send text="hi" client_id=cli > /tmp/fk_proxy/fm_send.json
jq -e '.reason == "no_host"' /tmp/fk_proxy/fm_send.json

# history, interrupt, backend_state — all return no_host envelope
$FANTASTIC fm history client_id=cli > /tmp/fk_proxy/fm_hist.json
jq -e '.reason == "no_host"' /tmp/fk_proxy/fm_hist.json

$FANTASTIC fm interrupt client_id=cli > /tmp/fk_proxy/fm_int.json
jq -e '.reason == "no_host"' /tmp/fk_proxy/fm_int.json

$FANTASTIC fm backend_state > /tmp/fk_proxy/fm_state.json
jq -e '.reason == "no_host"' /tmp/fk_proxy/fm_state.json

# reflect → generic proxy_agent identity, host_registered: false
$FANTASTIC fm reflect > /tmp/fk_proxy/fm_reflect.json
jq -e '.kind == "proxy_agent"' /tmp/fk_proxy/fm_reflect.json
jq -e '.host_registered == false' /tmp/fk_proxy/fm_reflect.json
```

Expect: every chat verb routes correctly at the bundle level; once a
Swift host is registered (in production), `handle(payload_json)` is
where the actual chat backend logic fires.

### Test 8: cargo tests (the bidirectional + streaming path)

```bash
cd /path/to/fantastic_canvas/rust
cargo test -p fantastic-proxy-agent
cargo run -p fantastic-proxy-agent --example proxy_mock_session
```

Expect:
- 20 unit tests pass (includes 5 chat-backend pattern tests)
- Example demo prints JSON at each step, ALL ASSERTIONS GREEN

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. cold-boot creates proxy_agent |  |  |
| 2. reflect host_registered=false |  |  |
| 3. boot ok |  |  |
| 4. arbitrary verb no_host |  |  |
| 5. shutdown ok |  |  |
| 6. cascade delete |  |  |
| 7. chat-backend verb shape |  |  |
| 8. cargo tests + example |  |  |

## End-to-end with a Swift host (manual, after app-side wires up)

```swift
let kernel = try await startKernelInMemory(portHint: 0)
_ = try await kernel.sendJson(targetId: "core",
    payloadJson: #"{"type":"create_agent","handler_module":"proxy_agent.tools","id":"chat_ui"}"#)
try kernel.registerProxyAgent(agentId: "chat_ui", host: ChatUIHost())

// Other agents address the UI:
_ = try await kernel.sendJson(targetId: "chat_ui",
    payloadJson: #"{"type":"render_token","delta":"Paris"}"#)
// → ChatUIHost.handle fires; SwiftUI re-renders on MainActor

// UI sends WITH sender attribution:
_ = try await kernel.sendJsonAs(senderId: "chat_ui",
    targetId: "chat",
    payloadJson: #"{"type":"send","text":"Hi"}"#)
// → state event carries sender="chat_ui"

// UI broadcasts state:
try await kernel.proxyEmit(agentId: "chat_ui",
    eventJson: #"{"type":"focus_changed","focused":true}"#)
// → watchers of chat_ui receive the event
```
