# foundation_models_backend selftest (Rust)

> scopes: ai, on-device, packaging
> requires: macOS with Xcode and a macOS 26+ SDK (the build runs `swift build`);
> for Test 2, macOS 26 or later with Apple Intelligence turned on (the on-device
> model downloaded)

Apple's on-device Foundation Models system model as an AI agent — no server, no
endpoint, no key, no model choice. The rust bundle links the swift
`FantasticFMCore` through the static C ABI `FantasticFMBridge`
(`../swift/Sources/FantasticFMBridge/fm_bridge.h`), so the texts below are the
swift bundle's too (its twin: `../swift/selftest/foundation_models_backend.md`).

## Pre-flight

ASK the user before Test 2: is this macOS 26+ with Apple Intelligence on? On
anything else Test 2 fails naming the reason (`device not eligible` / `Apple
Intelligence is not enabled` / `model not ready` / `platform not macOS 26+`) —
expected there, not a regression. Tests 1, 3 and 4 run no prompt.

```bash
cd src/lib/rust
cargo build -p fantastic-foundation-models-backend --tests
```

## Tests

### Test 1: the hermetic suite

```bash
cargo test -p fantastic-foundation-models-backend 2>&1 | grep "test result"
```
Expected (measured 2026-10-02, macOS 27.0, M1 Pro): `32 passed; 0 failed` in
under a second. A fake engine drives the bundle (schema, write refusals, reflect,
the messages→request split, whole kernel turns, a kernel `cancel` reaching the
on-device stream), and the REAL bridge answers the pre-model refusals (a
malformed `request_json`, an invalid turn or option, the NULL / unknown-handle
no-ops). `the_pinned_texts_are_the_swift_bundles_words` finds every refusal and
the schema hints verbatim in the swift sources.

### Test 2: the opt-in live test — tiny real prompts

```bash
FM_LIVE=1 cargo test -p fantastic-foundation-models-backend live_on_device_model \
  -- --nocapture 2>&1 | grep FM_LIVE
```
Expected (measured 2026-10-02, 4.7 s):
- `FM_LIVE availability=Available context_size=4096`
- `FM_LIVE tiny: text="Pong." … error=None prompt_tokens=67`
- `FM_LIVE transcript: text="Zed." error=None prompt_tokens=84` — the replayed
  turns reached the model
- `FM_LIVE cancel: ended after 4 deltas / 9 bytes, done=Some(Some("cancelled"))`
- `FM_LIVE kernel send: {"response":"Hello. <call name=\"done\"></call>",…}
  reported_prompt_tokens=79` — one turn through a kernel agent with a short
  `system_prompt`
- `FM_LIVE default system block: {"error":"foundation_models_backend:
  context_insufficient — the system prompt (5815 tok) leaves no room in the
  4096-token window …"}` — the kernel's default system block does not fit the
  on-device window; the model is not called

Regression signal: an `error=` other than `None`, or a `kernel send` carrying
`error`, on an available model.

### Test 3: the binary links the bridge

```bash
cargo build --bin fantastic_kernel
otool -L target/debug/fantastic_kernel | grep -E "FoundationModels|swift_Concurrency"
otool -l target/debug/fantastic_kernel | grep -A3 LC_BUILD_VERSION | grep minos
```
Expected: `FoundationModels.framework … weak`, `/usr/lib/swift/libswift_Concurrency.dylib`
(not `@rpath/…`), `minos 14.0`.

### Test 4: the catalog and the write refusal, through the binary

```bash
mkdir -p /tmp/fa_fm_rs && cd /tmp/fa_fm_rs && rm -rf .fantastic
BIN=/path/to/src/lib/rust/target/debug/fantastic_kernel
$BIN kernel reflect bundles=all tree=none | python3 -c "
import sys,json
fm=[b for b in json.load(sys.stdin)['bundles'] if b['name']=='foundation_models_backend'][0]
print([(f['name'], f.get('default')) for f in fm['schema']['fields']])"
$BIN kernel_state create_agent parent_id=kernel_state handler_module=foundation_models_backend.tools \
  id=bad memory_store_id=mem model=x
```
Expected (measured 2026-10-02):
- `[('temperature', None), ('max_tokens', None), ('memory_store_id', None),
  ('context_window', 4096), ('output_reserve', 1024), ('context_strategy',
  'compact'), ('recent_n', 6)]`
- `"error": "foundation_models_backend: \`model\` has no meaning here — the model
  is Apple's on-device system model (no endpoint, no model choice, no
  credentials)"`, `"reason": "invalid_meta"`

## Summary

| Test | Name | Pass/Fail | Notes |
|---|---|---|---|
| 1 | hermetic suite | | |
| 2 | live on-device prompts | | |
| 3 | binary links the bridge | | |
| 4 | catalog + write refusal | | |
