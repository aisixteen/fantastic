# proxy_agent selftest

> scopes: bundle wire-shape, graceful-degrade
> requires: `cargo build --release --bin fantastic`
> out-of-scope: bidirectional streaming (no embedding host in the CLI)

CLI-driven smoke for the verb wire shape. The streaming +
bidirectional path is covered by the cargo unit tests and the
runnable example (`cargo run -p fantastic-proxy-agent --example
proxy_mock_session`), since the CLI has no embedding host wired in.

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

Expect: structured error — the embedding host / UI uses `reason` to
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

Verifies the wire shape the LLM chat backend speaks. CLI
process has no embedding host registered, so every verb returns the
graceful `no_host` envelope — that's exactly what proves the
verbs are reaching the bundle. Multi-step state verification
happens via cargo unit tests + the runnable example (the CLI
can't drive an embedding host).

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

Expect: every chat verb routes correctly at the bundle level; once an
embedding host is registered (in production), `handle(payload_json)` is
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

## End-to-end with an embedding host (manual, once a host is wired)

A host is plain Rust: an implementor of the bundle's host trait
(the in-process app, or a test mock like `proxy_mock_session`).
Once `create_agent handler_module=proxy_agent.tools id=chat_ui`
has run, the embedding app registers its host against that agent
id. From then on the verb/emit flow is:

- **Inbound verbs.** Other agents address the UI agent (e.g. a
  `render_token` verb with `delta="Paris"`). The bundle forwards
  the payload to the registered host's `handle(payload_json)`,
  which the app uses to update its own view state. With no host
  registered this same verb returns the `no_host` envelope (Test 4).
- **Outbound with sender attribution.** The UI agent sends a verb
  to another agent (e.g. `send text="Hi"` to `chat`) carrying its
  own id as sender, so the resulting state event records
  `sender="chat_ui"`.
- **Broadcast emit.** The host emits an event (e.g.
  `focus_changed focused=true`) that the bundle fans out to every
  watcher of `chat_ui`.

The cargo unit tests + the `proxy_mock_session` example exercise
this full inbound/outbound/emit loop against a plain-Rust mock host.
