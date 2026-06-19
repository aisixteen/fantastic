# tools selftest

> scopes: verb wire shape, single-shot CLI surface
> requires: `cargo build --release --bin fantastic`
> out-of-scope: multi-call state verification — the registry is
> process-global in-memory; each CLI invocation is a fresh process
> with an empty registry. Multi-step register / list / dispatch /
> unregister flow is covered by the cargo unit tests AND the
> runnable example.

The CLI selftest verifies the verb wire shape one call at a time. To
verify state persistence across operations (register → list →
dispatch → unregister) use:

```bash
cargo test -p fantastic-tools
cargo run -p fantastic-tools --example tools_mock_session
```

Same constraint applies to `proxy_agent`'s host registry — see its
selftest for the precedent.

## Pre-flight

```bash
rm -rf /tmp/fk_tools && mkdir -p /tmp/fk_tools && cd /tmp/fk_tools
FANTASTIC=/path/to/rust/target/release/fantastic
```

## Tests

### Test 1: create the tools agent (disk-persisted)

```bash
$FANTASTIC reflect >/dev/null
$FANTASTIC core create_agent handler_module=tools.tools id=tools \
  > /tmp/fk_tools/create.json
jq -e '.id == "tools"' /tmp/fk_tools/create.json
test -d .fantastic/agents/tools
jq -e '.handler_module == "tools.tools"' .fantastic/agents/tools/agent.json
```

Expect: standard create_agent flow + record persisted in
`.fantastic/agents/tools/agent.json`.

### Test 2: reflect — empty registry, verb table well-formed

```bash
$FANTASTIC tools reflect > /tmp/fk_tools/reflect.json
jq -e '.kind == "tools"' /tmp/fk_tools/reflect.json
jq -e '.tool_count == 0' /tmp/fk_tools/reflect.json
jq -e '.verbs.register | type == "string"' /tmp/fk_tools/reflect.json
jq -e '.verbs.dispatch | type == "string"' /tmp/fk_tools/reflect.json
jq -e '.verbs.list_for_llm | type == "string"' /tmp/fk_tools/reflect.json
jq -e '.verbs.unregister_by_sender | type == "string"' /tmp/fk_tools/reflect.json
```

Expect: bundle self-describes with `tool_count == 0`, full verb
table.

### Test 3: register — minimum payload, returns ok

```bash
$FANTASTIC tools register \
  name=ping \
  agent_id=health_check \
  description="Returns pong." \
  parameters_schema='{"type":"object","properties":{},"additionalProperties":false}' \
  > /tmp/fk_tools/reg_min.json
jq -e '.ok == true' /tmp/fk_tools/reg_min.json
jq -e '.name == "ping"' /tmp/fk_tools/reg_min.json
```

Expect: minimum payload registers cleanly within this process.
(Verifying it survived to a separate `list` call requires daemon
mode or in-process testing — see cargo tests for that path.)

### Test 4: register — light-max payload (verb + sender + rich schema)

```bash
$FANTASTIC tools register \
  name=search \
  agent_id=doc_index \
  verb=search \
  sender=docs_owner \
  description="Search internal docs." \
  parameters_schema='{"type":"object","properties":{"query":{"type":"string","minLength":1},"limit":{"type":"integer","default":5,"maximum":50},"scope":{"type":"string","enum":["all","engineering","policy"],"default":"all"}},"required":["query"],"additionalProperties":false}' \
  > /tmp/fk_tools/reg_max.json
jq -e '.ok == true' /tmp/fk_tools/reg_max.json
jq -e '.name == "search"' /tmp/fk_tools/reg_max.json
```

Expect: rich schema accepted and parsed (string → object).

### Test 5: dispatch unknown tool → tool_not_found

```bash
$FANTASTIC tools dispatch name=never_registered arguments='{}' \
  > /tmp/fk_tools/disp_404.json
jq -e '.reason == "tool_not_found"' /tmp/fk_tools/disp_404.json
jq -e '.error | contains("never_registered")' /tmp/fk_tools/disp_404.json
```

Expect: registry returns structured `tool_not_found` (the registry
is empty in this process). Verifies the dispatch wire shape + error
envelope.

### Test 6: unregister unknown → not_found

```bash
$FANTASTIC tools unregister name=nope > /tmp/fk_tools/unreg_404.json
jq -e '.reason == "not_found"' /tmp/fk_tools/unreg_404.json
```

### Test 7: unregister_by_sender — empty registry returns removed=0

```bash
$FANTASTIC tools unregister_by_sender sender=anyone \
  > /tmp/fk_tools/unreg_sender.json
jq -e '.ok == true' /tmp/fk_tools/unreg_sender.json
jq -e '.removed == 0' /tmp/fk_tools/unreg_sender.json
jq -e '.sender == "anyone"' /tmp/fk_tools/unreg_sender.json
```

Expect: graceful — no error when nothing to remove.

### Test 8: unregister_by_sender without sender field → invalid_args

```bash
$FANTASTIC tools unregister_by_sender > /tmp/fk_tools/unreg_invalid.json
jq -e '.reason == "invalid_args"' /tmp/fk_tools/unreg_invalid.json
jq -e '.error | contains("sender")' /tmp/fk_tools/unreg_invalid.json
```

### Test 9: cascade delete

```bash
$FANTASTIC core delete_agent id=tools > /tmp/fk_tools/del.json
jq -e '.deleted == true' /tmp/fk_tools/del.json
test ! -e .fantastic/agents/tools
```

Expect: standard cascade. Bundle's `on_delete` hook clears the
in-memory registry (no observable effect at the CLI level since
each call has its own registry).

### Test 10: cargo tests + runnable example (the multi-step path)

```bash
cd /path/to/fantastic_canvas/rust
cargo test -p fantastic-tools          # 23 unit tests
cargo run -p fantastic-tools --example tools_mock_session
```

Expect:
- 23 unit tests pass — covers register/list/dispatch state across
  operations in-process
- Example demo prints JSON at each step, ALL ASSERTIONS GREEN

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. create tools agent | | |
| 2. reflect empty | | |
| 3. register min ok | | |
| 4. register max ok | | |
| 5. dispatch unknown → tool_not_found | | |
| 6. unregister unknown → not_found | | |
| 7. unregister_by_sender empty | | |
| 8. unregister_by_sender invalid_args | | |
| 9. cascade delete | | |
| 10. cargo + example | | |

## End-to-end with a chat backend (manual, after the app wires up)

The chat backend is a `proxy_agent.tools` agent backed by an
embedding-host LLM impl — a plain-Rust implementor of the proxy-agent
trait (the in-process app, or a test mock). The same agent answers
every chat verb (`send` / `history` / `interrupt` / `backend_state` /
`reflect`) and pulls tools from this registry inside its `send`
handler.

```bash
# 1. Create the tools agent + the chat backend (proxy_agent) agent.
$FANTASTIC core create_agent handler_module=tools.tools id=tools
$FANTASTIC core create_agent handler_module=proxy_agent.tools id=fm

# 2. Register the host LLM impl in process: kernel.register_proxy_agent("fm", host).
#    The host's handle({type:"send"}) pulls list_for_llm internally and
#    streams tokens back as emit events on the fm agent's own inbox.

# 3. Register tools (convenience wrapper or raw send).
$FANTASTIC tools register \
  sender=weather_provider \
  name=get_weather \
  agent_id=weather_provider \
  description="Returns the current weather for a city." \
  parameters_schema='{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}'

# 4. Chat now flows through the fm agent. The web-app chat consumer
#    talks to "fm" the same way it would talk to any chat backend.
$FANTASTIC fm send text="What's the weather in Paris?" client_id=cli
# → host pulls list_for_llm → builds an LLM session with tools
# → streams tokens via emit events on "fm" ({type:"token", ...})
# → the web-app's watcher receives them and re-renders

# 5. Cleanup on logout / mode change.
$FANTASTIC tools unregister_by_sender sender=weather_provider
```
