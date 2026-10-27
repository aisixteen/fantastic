# fantastic-agentic-resources selftest

> scopes: kernel, roles
> requires: `cargo build --release --bin fantastic_kernel`
> out-of-scope: a live model behind a tier — the cards and the limits are exercised
> against any bundle a tier names

A generic roles registry. **Off by default**: the host turns it on with
`FANTASTIC_AGENTIC_RESOURCES=1`, and the registry agent's record carries its config
(`role_dirs`, `tiers`). One-shot CLI: `fantastic <id> <verb> [k=v ...]`; JSON-valued
args are coerced.

## Pre-flight

All test state lives under `/tmp/ar_test/`.

```bash
rm -rf /tmp/ar_test && mkdir -p /tmp/ar_test/roles && cd /tmp/ar_test
export FANTASTIC_AGENTIC_RESOURCES=1
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
cat > roles/writer.md <<'CARD'
---
name: writer
purpose: Writes the first draft of a text.
tier: fast
---
You write clearly and briefly.
CARD
AR=$($FANTASTIC kernel_state create_agent handler_module=agentic_resources.tools \
  role_dirs='["/tmp/ar_test/roles"]' \
  tiers='{"fast":{"handler_module":"scheduler.tools"}}' | jq -r .id)
```

## Tests

### Test 1: roles lists the cards, the shipped project_lead included

```bash
$FANTASTIC $AR roles | jq -e '[.roles[].name] | contains(["writer","project_lead"])'
```

### Test 2: role returns the full card; an unknown name is not_found

```bash
$FANTASTIC $AR role name=writer | jq -e '.directive == "You write clearly and briefly."'
$FANTASTIC $AR role name=ghost | jq -e '.reason == "not_found"'
```

### Test 3: a cap refuses the spawn past it

```bash
$FANTASTIC $AR set_limit role=writer max=1 | jq -e '.ok'
$FANTASTIC $AR spawn role=writer | jq -e '.id'
$FANTASTIC $AR spawn role=writer | jq -e '.error == "limit" and .scope == "role"'
$FANTASTIC $AR get_count role=writer | jq -e '.running == 1 and .max == 1'
```

### Test 4: fleet keeps every spawned agent (a paused or done one only frees its slot); delete is the only way out

```bash
$FANTASTIC $AR fleet | jq -e '.roles.writer | length == 1'
```

## Cleanup

```bash
$FANTASTIC kernel_state delete_agent id=$AR
rm -rf /tmp/ar_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. roles | | |
| 2. role / not_found | | |
| 3. limit refuses | | |
| 4. fleet keeps finished | | |
