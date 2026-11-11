# fantastic-core selftest

> scopes: substrate
> requires: `cargo build --release --bin fantastic_kernel`
> out-of-scope: HTTP, WS, bundles other than core itself

Root orchestrator. System verbs (create_agent / delete_agent /
update_agent / list_agents) + reflect. No
handler_module — dispatch is native to the Agent class.

## Pre-flight

```bash
rm -rf /tmp/fc_test
mkdir -p /tmp/fc_test
cd /tmp/fc_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
```

## Tests

### Test 1: core agent exists after cold boot

```bash
$FANTASTIC reflect | jq -e '.tree.id == "kernel_state"'
test -f .fantastic/agent.yaml
jq -e '.id == "kernel_state"' .fantastic/agent.yaml
```

### Test 2: list_agents enumerates everything

```bash
$FANTASTIC kernel_state create_agent handler_module=file_bridge.tools id=t_file
$FANTASTIC kernel_state list_agents | jq -e '.agents | length >= 2'
$FANTASTIC kernel_state list_agents | jq -e '[.agents[].id] | contains(["kernel_state","t_file"])'
```

### Test 3: update_agent merges meta + persists

```bash
$FANTASTIC kernel_state update_agent id=t_file note="from test 4"
jq -e '.note == "from test 4"' .fantastic/agents/t_file/agent.yaml
```

### Test 4: delete_lock blocks delete_agent

```bash
$FANTASTIC kernel_state update_agent id=t_file delete_lock=true
$FANTASTIC kernel_state delete_agent id=t_file | jq -e '.locked == true and .id == "t_file"'
test -d .fantastic/agents/t_file
# Clear lock and delete cleanly:
$FANTASTIC kernel_state update_agent id=t_file delete_lock=false
$FANTASTIC kernel_state delete_agent id=t_file | jq -e '.deleted == true'
test ! -e .fantastic/agents/t_file
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. core exists after cold boot |  |  |
| 2. list_agents enumerates |  |  |
| 3. update_agent persists |  |  |
| 4. delete_lock blocks delete |  |  |
