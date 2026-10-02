/*
 * fm_bridge.h — the C ABI of libFantasticFMBridge.a (Swift target
 * FantasticFMBridge, over FantasticFMCore): Apple's on-device Foundation
 * Models, for a runtime that is not Swift (the rust foundation_models_backend).
 *
 * ONE implementation of the FoundationModels calls lives behind this header;
 * the swift bundle calls the same Swift core directly. Text in and text out:
 * no tools, no guided generation — the caller's ai_core owns the <call>
 * grammar.
 *
 * Build:  swift build -c release --product FantasticFMBridge
 *         -> .build/release/libFantasticFMBridge.a
 *
 * ------------------------------------------------------------------------
 * OWNERSHIP
 *   - fm_availability() returns a heap string the CALLER owns: release it with
 *     fm_free(). fm_free(NULL) is a no-op. Never release it with free() from
 *     another allocator domain.
 *   - request_json is BORROWED for the duration of fm_stream() only; it is
 *     copied before fm_stream() returns.
 *   - ctx is the caller's. The bridge never dereferences or frees it; it only
 *     passes it back. It must stay valid until on_done has been called.
 *   - `bytes` in on_chunk and `error` in on_done are BORROWED for the duration
 *     of that callback only: copy what you keep, never free them.
 *
 * THREADING
 *   - Callbacks run on a Swift-concurrency pool thread — never the thread that
 *     called fm_stream() — and MAY begin before fm_stream() has returned.
 *   - For one handle the calls are SERIAL and IN ORDER: on_chunk zero or more
 *     times, then on_done EXACTLY ONCE, last. That holds on success, on every
 *     failure, and after fm_cancel(). on_done is where ctx may be released.
 *   - Callbacks may call fm_cancel() and fm_free(). They should return
 *     promptly: each holds a pool thread while it runs.
 *
 * ERRORS
 *   Every failure after a handle is issued arrives as on_done's `error`
 *   (UTF-8, NUL-terminated): "cancelled" after fm_cancel(); otherwise a
 *   sentence — an unavailable model ("the on-device model is unavailable:
 *   <reason>"), a malformed request ("request_json: ..."), an invalid turn or
 *   option, or a generation failure ("the on-device model failed: ..."). The
 *   bundles prefix "foundation_models_backend: " and raise it unchanged.
 * ------------------------------------------------------------------------
 */
#ifndef FANTASTIC_FM_BRIDGE_H
#define FANTASTIC_FM_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* {"available":true,"reason":null} or {"available":false,"reason":"<sentence>"}
 * — keys in this order, no whitespace. The reason names one of: device not
 * eligible, Apple Intelligence not enabled, model not ready, platform not
 * macOS 26+ (or a reason the SDK added later, as the SDK prints it).
 * Heap-allocated; release with fm_free(). */
char *fm_availability(void);

/* The on-device model's context window in tokens, read from the SDK
 * (SystemLanguageModel.contextSize). Where no SDK can be asked (not macOS 26+,
 * where every stream is refused) it is Apple's documented 4096 (TN3193). */
int64_t fm_context_size(void);

/* on_chunk: `len` > 0 bytes of UTF-8 text — a DELTA (new text only), not
 * NUL-terminated, borrowed for the call. */
typedef void (*fm_chunk_fn)(void *ctx, const uint8_t *bytes, size_t len);

/* on_done: error == NULL on success, else a borrowed NUL-terminated message.
 * prompt_tokens: the model's own count of the whole prompt (instructions +
 * turns + prompt), or -1 when the SDK reports none (before macOS 27). */
typedef void (*fm_done_fn)(void *ctx, const char *error, int64_t prompt_tokens);

/* Start one generation. request_json (UTF-8, NUL-terminated):
 *   {"instructions": str,                      required ("" for none)
 *    "turns": [{"role": "user"|"assistant",    required ([] for none),
 *               "content": str}, ...],          replayed in order
 *    "prompt": str,                            required — the text answered
 *    "temperature": number >= 0 | null,        optional — null/absent = model's own
 *    "max_tokens": int >= 1 | null}            optional — null/absent = model's own
 * Other keys are ignored. Returns a handle >= 1; returns 0 ONLY when on_chunk
 * or on_done is NULL, and then nothing is ever called back. */
uint64_t fm_stream(const char *request_json, void *ctx, fm_chunk_fn on_chunk,
                   fm_done_fn on_done);

/* Cancel a live generation; its on_done follows with "cancelled". An unknown
 * or finished handle is a no-op. */
void fm_cancel(uint64_t handle);

/* Release a string from fm_availability(). NULL is a no-op. */
void fm_free(char *p);

#ifdef __cplusplus
}
#endif

#endif /* FANTASTIC_FM_BRIDGE_H */
