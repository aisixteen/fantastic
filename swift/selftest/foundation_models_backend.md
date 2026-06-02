# foundation_models_backend selftest (Swift)

> requires: macOS 26 + Apple Silicon (FoundationModels)
> scopes: ai, apple
> out-of-scope: a live on-device generation (model-dependent; structural
> assertions only here) — chat UI flow (now the TS `ai_view` in `ts/`)

Apple on-device Foundation Models LLM backend. Same LLM-backend verb
surface as `ollama_backend` / `nvidia_nim_backend`, wrapping Apple's
`FoundationModels` (`LanguageModelSession`) framework. ATOMIC +
STATELESS — every `send` builds a fresh session, feeds one user
message, streams, drops it. No Python equivalent.

These tests are **structural only**: they assert the verb surface and
the availability snapshot, not a live token stream. The on-device model
may be unavailable (Apple Intelligence not enabled, device not eligible,
model not downloaded) — `backend_state` reports that cleanly and the
tests still pass.

## Pre-flight

The Swift root agent id is `core`. State lives under `/tmp/fa_fmtest/`.

```bash
BIN=/Users/oleksandr/Projects/fantastic_canvas/swift/.build/debug/fantastic
rm -rf /tmp/fa_fmtest && mkdir -p /tmp/fa_fmtest && cd /tmp/fa_fmtest

# Create the FM backend agent; capture its id.
FM=$("$BIN" core create_agent handler_module=foundation_models_backend.tools \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['id'])")
echo "FM=$FM"
```
Expected: `FM=foundation_models_backend_<hex6>`.

## Tests

### Test 1: reflect lists the four backend verbs

```bash
"$BIN" "$FM" reflect | python3 -c "
import json,sys
d = json.load(sys.stdin)
verbs = set(d.get('verbs', {}))
need = {'send','history','interrupt','backend_state'}
print('kind:', d.get('kind'))
print('provider:', d.get('provider'))
print('PASS' if need <= verbs else f'FAIL missing={need - verbs}')
"
```
Expected: `kind: foundation_models_backend`,
`provider: apple_foundation_models`, and `PASS` — reflect's `verbs`
map carries `send`, `history`, `interrupt`, and `backend_state`.
Regression signal: a missing verb means the dispatch table drifted
from the ollama/nvidia LLM-backend contract.

### Test 2: backend_state reports availability

```bash
"$BIN" "$FM" backend_state | python3 -c "
import json,sys
d = json.load(sys.stdin)
keys = {'provider','apple_intelligence_available','model_available',
        'backend_registered','model','in_flight','reason'}
print('available:', d.get('model_available'), 'reason:', d.get('reason'))
ok = (keys <= set(d)
      and d['provider'] == 'apple_foundation_models'
      and d['backend_registered'] is True
      and isinstance(d['model_available'], bool)
      and d['in_flight'] == 0)
print('PASS' if ok else f'FAIL d={d}')
"
```
Expected: `PASS`. `backend_state` is a structural snapshot —
`provider` is `apple_foundation_models`, `backend_registered` is
`true`, `model_available` is a bool (whatever the device reports),
`in_flight` is `0` (no send issued), and `reason` is `ok` when
available or a machine-readable cause
(`apple_intelligence_not_enabled` / `device_not_eligible` /
`model_not_ready` / `os_version_too_old` / `framework_not_available`)
when not.

Note: when `model_available` is `false`, `reflect`'s `available` field
matches and a `send` returns
`{"error":"foundation_models_unavailable","reason":<same>}` rather than
streaming. Asserting that failfast is fine here; do NOT mark Test 2 a
fail just because the device has no model.

## Summary

| # | Test | Pass |
|---|------|------|
| 1 | reflect lists send/history/interrupt/backend_state | |
| 2 | backend_state reports availability snapshot | |

Also report:
- `model_available` and `reason` from Test 2 (was the on-device model
  actually reachable, or structural-only?).
