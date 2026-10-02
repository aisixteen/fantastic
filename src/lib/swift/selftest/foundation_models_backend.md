# foundation_models_backend selftest (Swift)

> scopes: ai, on-device, packaging
> requires: macOS 26 or later with Apple Intelligence turned on (the on-device
> model downloaded); Xcode command-line tools (`clang`, `swift`)

Apple's on-device Foundation Models system model as an AI agent. No server, no
endpoint, no key, no model choice. The FoundationModels calls live in
`FantasticFMCore`; the rust kernel links the same code through the static C ABI
`FantasticFMBridge` (`Sources/FantasticFMBridge/fm_bridge.h`), so the texts below
are the rust bundle's too.

## Pre-flight

ASK the user before running: is this macOS 26+ with Apple Intelligence on? On
anything else Tests 1-2 print the refusal reason (`"available":false` with
`device not eligible` / `Apple Intelligence is not enabled` / `model not ready` /
`platform not macOS 26+`) — expected there, not a regression.

```bash
cd /Users/oleksandr/Projects/fantastic_canvas/src/lib/swift
swift build --build-tests
```

## Tests

### Test 1: the opt-in live test — the whole stack on the real model

```bash
FM_LIVE=1 swift test --filter liveOnDeviceModel 2>&1 | grep FM_LIVE
```
Expected (measured 2026-10-02, macOS 27.0, M1 Pro, 4.2 s):
- `availability={"available":true,"reason":null} context_size=4096 source=sdk`
- `transcript: text="Zed." …` — the replayed turns reached the model
- `C ABI: text="Pong." error=nil prompt_tokens=67 …`
- `cancel: ended after 7 deltas …` — the count did not finish
- `reflect: available=true context_window=4096 context_size=4096`
- `kernel send: {"response":"Hello. <call name=\"done\"></call>",…}` — one turn
  through a kernel agent, with a lean `system_prompt`

Regression signal: an `error=` other than `nil`, or a `kernel send` carrying
`error`, on an available model.

### Test 2: the C ABI from plain C — what the rust bundle links

```bash
swift build -c release --product FantasticFMBridge
mkdir -p /tmp/fa_fm && cd /tmp/fa_fm
cat > main.c <<'EOF'
#include <stdio.h>
#include <string.h>
#include <pthread.h>
#include "fm_bridge.h"
typedef struct { pthread_mutex_t m; pthread_cond_t c; int done; char t[4096]; size_t n; char e[512]; int64_t pt; } ctx_t;
static void on_chunk(void *p, const uint8_t *b, size_t len) { ctx_t *x = p; pthread_mutex_lock(&x->m); if (x->n + len < sizeof x->t) { memcpy(x->t + x->n, b, len); x->n += len; } pthread_mutex_unlock(&x->m); }
static void on_done(void *p, const char *e, int64_t pt) { ctx_t *x = p; pthread_mutex_lock(&x->m); if (e) snprintf(x->e, sizeof x->e, "%s", e); x->pt = pt; x->done = 1; pthread_cond_signal(&x->c); pthread_mutex_unlock(&x->m); }
static void run(const char *req) {
  ctx_t x; memset(&x, 0, sizeof x); pthread_mutex_init(&x.m, 0); pthread_cond_init(&x.c, 0);
  uint64_t h = fm_stream(req, &x, on_chunk, on_done);
  pthread_mutex_lock(&x.m); while (!x.done) pthread_cond_wait(&x.c, &x.m); pthread_mutex_unlock(&x.m);
  printf("handle=%llu text=\"%.*s\" error=%s prompt_tokens=%lld\n", (unsigned long long)h, (int)x.n, x.t, x.e[0] ? x.e : "(null)", (long long)x.pt);
}
int main(void) {
  char *a = fm_availability(); printf("fm_availability=%s\n", a); fm_free(a);
  printf("fm_context_size=%lld\n", (long long)fm_context_size());
  printf("null-callback handle=%llu\n", (unsigned long long)fm_stream("{}", 0, 0, 0));
  run("{\"instructions\":\"Answer with one word.\",\"turns\":[{\"role\":\"user\",\"content\":\"My name is Zed.\"},{\"role\":\"assistant\",\"content\":\"Noted.\"}],\"prompt\":\"What is my name?\",\"temperature\":0,\"max_tokens\":16}");
  run("{\"turns\":[],\"prompt\":\"x\"}");
  return 0;
}
EOF
SWIFT=/Users/oleksandr/Projects/fantastic_canvas/src/lib/swift
clang -I"$SWIFT/Sources/FantasticFMBridge" main.c "$SWIFT/.build/release/libFantasticFMBridge.a" \
  -L"$(xcrun --show-sdk-path)/usr/lib/swift" -mmacosx-version-min=14.0 -o smoke
otool -L smoke | grep FoundationModels
./smoke
```
Expected (measured 2026-10-02): `FoundationModels … weak` (the binary still loads
on a macOS without the framework), then
```
fm_availability={"available":true,"reason":null}
fm_context_size=4096
null-callback handle=0
handle=1 text="Zed." error=(null) prompt_tokens=84
handle=2 text="" error=request_json: `instructions` is missing prompt_tokens=-1
```
No `-l` flags are needed beyond the archive: its objects carry autolink entries
for the Swift runtime and FoundationModels. Link for macOS 14+ (the objects'
deployment target); an older one links with a version warning.

### Test 3: the hermetic suite (no model needed)

```bash
swift test --no-parallel --filter FantasticFMTests
```
Expected: 41 tests, 40 passed and `liveOnDeviceModel` skipped. They pin, byte
for byte, what the rust bundle must match: the record schema, every write
refusal, the reflect extras, the messages→request split and its refusal, the
unavailable-model refusal, and the C ABI's callbacks, handles and cancel.

## Known posture (measured, not a failure)

The kernel's default system block does not fit the on-device window: 22,590
chars, 5,804 tokens by the kernel's estimate, 6,149 by the model's own count,
against 4,096. A `send` without a lean `system_prompt` fails fast with
`context_insufficient … the 4096-token window` and the model is not called.
