# fantastic-nvidia-nim-backend selftest

> scopes: kernel, ai, persistence, http
> requires: `cargo build --release --bin fantastic`; an `NVAPI_KEY`
> env var (`nvapi-...` from https://build.nvidia.com) for any AI test.
> out-of-scope: live single-shot generation + per-client thread
> persistence + status phase machine — those need a live daemon + WS
> (see the python spec, marked AI); chat UI flow (TS `ai_view` in `ts/`)

NVIDIA NIM LLM agent (OpenAI-compatible). Same surface as
`ollama_backend` (send/history/interrupt) plus `set_api_key`/
`clear_api_key`. The api_key is a sidecar at
`.fantastic/agents/<id>/api_key` via `file_agent_id` — never in
`agent.json`, never returned by reflect. The failfast guards below
run as one-shot CLI; AI generation needs a live daemon + WS.

## Pre-flight

All test state lives under `/tmp/nim_test/`.

```bash
rm -rf /tmp/nim_test && mkdir -p /tmp/nim_test && cd /tmp/nim_test
FANTASTIC=/path/to/rust/target/release/fantastic
FA=$($FANTASTIC core create_agent handler_module=file_bridge.tools root=/tmp/nim_test | jq -r .id)
NB=$($FANTASTIC core create_agent handler_module=nvidia_nim_backend.tools file_agent_id=$FA | jq -r .id)
NB2=$($FANTASTIC core create_agent handler_module=nvidia_nim_backend.tools | jq -r .id)
```

## Tests

### Test 1: `send` failfast when `file_agent_id` unset

```bash
$FANTASTIC $NB2 send text=hi | jq -e '.error | contains("file_agent_id required")'
```

Expect: `{"error":"nvidia_nim_backend: file_agent_id required"}`.

### Test 2: `set_api_key` failfast when `file_agent_id` unset

```bash
$FANTASTIC $NB2 set_api_key api_key=nvapi-x | jq -e '.error | contains("file_agent_id required")'
```

### Test 3: `send` failfast when api_key not set (bound, no key)

```bash
$FANTASTIC $NB send text=hi | jq -e '.error | contains("api_key not set")'
```

Expect: `nvidia_nim_backend: api_key not set; call set_api_key first`.

### Test 4: `set_api_key` writes sidecar; reflect flips `has_api_key`, never leaks the key

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && {
  $FANTASTIC $NB set_api_key api_key="$NVAPI_KEY" | jq -e '.ok == true'
  test -f /tmp/nim_test/.fantastic/agents/$NB/api_key && echo "sidecar present"
  $FANTASTIC $NB reflect | jq -e '.has_api_key == true and (tostring | contains("nvapi-") | not)'
  $FANTASTIC $NB clear_api_key | jq -e '.ok == true and .deleted == true'
  test ! -f /tmp/nim_test/.fantastic/agents/$NB/api_key && echo "sidecar removed"
} || echo "SKIPPED (no NVAPI_KEY)"
```

Expect: `{"ok":true}`, sidecar exists, `has_api_key:true` with no
`nvapi-` substring anywhere in the reflect blob, then sidecar gone.

## Cleanup

```bash
$FANTASTIC core delete_agent id=$NB
$FANTASTIC core delete_agent id=$NB2
$FANTASTIC core delete_agent id=$FA
rm -rf /tmp/nim_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. send failfast w/o file_agent_id |  |  |
| 2. set_api_key failfast w/o file_agent_id |  |  |
| 3. send failfast w/o api_key |  |  |
| 4 (AI). set_api_key sidecar + reflect doesn't leak |  |  |
