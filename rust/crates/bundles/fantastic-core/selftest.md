# fantastic-core selftest

> scopes: substrate
> requires: `cargo build --release --bin fantastic`
> out-of-scope: HTTP, WS, bundles other than core itself

Root orchestrator. System verbs (create_agent / delete_agent /
update_agent / list_agents) + reflect + readme seeding. No
handler_module — dispatch is native to the Agent class.

## Pre-flight

```bash
rm -rf /tmp/fc_test
mkdir -p /tmp/fc_test
cd /tmp/fc_test
FANTASTIC=/path/to/rust/target/release/fantastic
```

## Tests

### Test 1: core agent exists after cold boot

```bash
$FANTASTIC reflect | jq -e '.tree.id == "core"'
test -f .fantastic/agent.json
jq -e '.id == "core"' .fantastic/agent.json
```

### Test 2: core seeds its own readme.md

```bash
test -f .fantastic/readme.md
grep -q "This is a Fantastic kernel" .fantastic/readme.md
grep -q "send(target_id, payload)" .fantastic/readme.md
```

### Test 3: list_agents enumerates everything

```bash
$FANTASTIC core create_agent handler_module=file.tools id=t_file
$FANTASTIC core list_agents | jq -e '.agents | length >= 2'
$FANTASTIC core list_agents | jq -e '[.agents[].id] | contains(["core","t_file"])'
```

### Test 4: update_agent merges meta + persists

```bash
$FANTASTIC core update_agent id=t_file note="from test 4"
jq -e '.note == "from test 4"' .fantastic/agents/t_file/agent.json
```

### Test 5: delete_lock blocks delete_agent

```bash
$FANTASTIC core update_agent id=t_file delete_lock=true
$FANTASTIC core delete_agent id=t_file | jq -e '.locked == true and .id == "t_file"'
test -d .fantastic/agents/t_file
# Clear lock and delete cleanly:
$FANTASTIC core update_agent id=t_file delete_lock=false
$FANTASTIC core delete_agent id=t_file | jq -e '.deleted == true'
test ! -e .fantastic/agents/t_file
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. core exists after cold boot |  |  |
| 2. readme.md auto-seeded |  |  |
| 3. list_agents enumerates |  |  |
| 4. update_agent persists |  |  |
| 5. delete_lock blocks delete |  |  |
