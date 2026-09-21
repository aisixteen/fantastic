# fantastic-nvidia-nim-backend selftest

> scopes: kernel, ai, persistence, http
> requires: `cargo build --release --bin fantastic_kernel`; an `NVAPI_KEY`
> env var (`nvapi-...` from https://build.nvidia.com) for any AI test.
> out-of-scope: live single-shot generation + per-client thread
> persistence + status phase machine — those need a live daemon + WS
> (see the python spec, marked AI); chat UI flow (TS `ai_view` in `ts/`)

NVIDIA NIM LLM agent (OpenAI-compatible). Same surface as
`ollama_backend` (send/history/status; an in-flight `send` is cancelled
with the kernel's `ps` + `cancel`, not a bundle `interrupt`). The api key comes
from the ENVIRONMENT (`NVIDIA_API_KEY`), read at call time — there is no
key-storage verb and nothing on disk, so it is never in `agent.json` and never
returned by reflect. The failfast guards below run as one-shot CLI; AI
generation needs a live daemon + WS.

## Pre-flight

All test state lives under `/tmp/nim_test/`.

```bash
rm -rf /tmp/nim_test && mkdir -p /tmp/nim_test && cd /tmp/nim_test
FANTASTIC=/path/to/rust/target/release/fantastic_kernel
FA=$($FANTASTIC kernel_state create_agent handler_module=file_bridge.tools root=/tmp/nim_test | jq -r .id)
NB=$($FANTASTIC kernel_state create_agent handler_module=nvidia_nim_backend.tools file_agent_id=$FA | jq -r .id)
NB2=$($FANTASTIC kernel_state create_agent handler_module=nvidia_nim_backend.tools | jq -r .id)
```

## Tests

### Test 1: `send` failfast when `file_agent_id` unset

```bash
$FANTASTIC $NB2 send text=hi | jq -e '.error | contains("file_agent_id required")'
```

Expect: `{"error":"nvidia_nim_backend: file_agent_id required"}`.

### Test 2: `send` failfast when the key variable is unset — and it NAMES the var

```bash
env -u NVIDIA_API_KEY $FANTASTIC $NB send text=hi | jq -e '.error | contains("NVIDIA_API_KEY")'
```

Expect: `nvidia_nim_backend: no api key — set NVIDIA_API_KEY`.

### Test 3: reflect flips `has_api_key` with the var set, and never leaks the key

Skip if `NVAPI_KEY` is unset.

```bash
[ -n "$NVAPI_KEY" ] && {
  NVIDIA_API_KEY="$NVAPI_KEY" $FANTASTIC $NB reflect \
    | jq -e '.has_api_key == true and (tostring | contains("nvapi-") | not)'
  test ! -e /tmp/nim_test/.fantastic/agents/$NB/api_key && echo "nothing written to disk"
} || echo "SKIPPED (no NVAPI_KEY)"
```

Expect `has_api_key:true` with no `nvapi-` substring anywhere in the reflect
blob, and NO sidecar file — the bundle stores nothing.

## Cleanup

```bash
$FANTASTIC kernel_state delete_agent id=$NB
$FANTASTIC kernel_state delete_agent id=$NB2
$FANTASTIC kernel_state delete_agent id=$FA
rm -rf /tmp/nim_test
```

## Summary table

| Test | Pass / Fail | Notes |
|---|---|---|
| 1. send failfast w/o file_agent_id |  |  |
| 2. send failfast w/o NVIDIA_API_KEY (names the var) |  |  |
| 3 (AI). reflect flips has_api_key, leaks nothing, writes nothing |  |  |
