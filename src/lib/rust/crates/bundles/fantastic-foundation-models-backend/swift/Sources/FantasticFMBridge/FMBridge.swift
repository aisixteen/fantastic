// FantasticFMBridge — the C ABI over FantasticFMCore, for rust.
//
// Shipped as a STATIC library product (`libFantasticFMBridge.a`); the rust
// `foundation_models_backend` links it, so both runtimes run the SAME
// FoundationModels calls (FMCore) and see the same refusal texts. The exact
// contract is `fm_bridge.h` beside this file; every rule below is restated
// there for a reader who never opens Swift.
//
// FIVE FUNCTIONS:
//   char    *fm_availability(void);   {"available":bool,"reason":str|null}; free with fm_free
//   int64_t  fm_context_size(void);   the model's window in tokens (from the SDK)
//   uint64_t fm_stream(request_json, ctx, on_chunk, on_done);   a handle, >= 1
//   void     fm_cancel(uint64_t handle);
//   void     fm_free(char *p);
//
// OWNERSHIP:
//   * fm_availability's string is the caller's: release it with fm_free.
//   * request_json is BORROWED for the call only — copied before fm_stream
//     returns, so the caller may free it right after.
//   * ctx is the caller's; the bridge never reads or frees it, only passes it
//     back. It must stay valid until on_done has run.
//   * the bytes on_chunk receives and the error string on_done receives are
//     BORROWED for the duration of that callback only — copy, never free.
//
// THREADING:
//   * callbacks run on a Swift-concurrency pool thread, never on the thread that
//     called fm_stream, and MAY start before fm_stream has returned;
//   * for one handle, calls are SERIAL and IN ORDER: on_chunk 0..n times, then
//     on_done EXACTLY ONCE, last — on success, on failure, and after fm_cancel
//     (error = "cancelled"). on_done is therefore where ctx may be released;
//   * a callback may call fm_cancel / fm_free; it should return promptly (it
//     holds a pool thread).

#if canImport(Darwin)
    import Darwin
#elseif canImport(Glibc)
    import Glibc
#elseif canImport(Musl)
    import Musl
#endif
import FantasticFMCore
import Foundation

/// `void (*on_chunk)(void *ctx, const uint8_t *bytes, size_t len)` — `len` > 0,
/// the bytes are UTF-8 text, not NUL-terminated, valid during the call only.
public typealias FMChunkCallback =
    @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, UInt) -> Void

/// `void (*on_done)(void *ctx, const char *error, int64_t prompt_tokens)` —
/// `error` NULL on success, else a NUL-terminated UTF-8 message (`"cancelled"`
/// after fm_cancel) valid during the call only; `prompt_tokens` the model's own
/// count of the prompt, or -1 when the SDK reported none (before macOS 27).
public typealias FMDoneCallback =
    @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int64) -> Void

/// The engine every C call reaches, and the live streams by handle.
enum FMBridge {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var _engine: any FMEngine = SystemFMEngine()
    nonisolated(unsafe) private static var tasks: [UInt64: Task<Void, Never>] = [:]
    nonisolated(unsafe) private static var nextHandle: UInt64 = 1

    /// The engine the C calls run on — the system model, unless a test installed
    /// a fake.
    static var engine: any FMEngine {
        get { lock.withLock { _engine } }
        set { lock.withLock { _engine = newValue } }
    }

    /// Start `body` as a stream task under a fresh handle. The registry lock is
    /// HELD while the task is stored, so a task that finishes at once cannot
    /// remove its handle before it was added.
    static func start(_ body: @escaping @Sendable (UInt64) async -> Void) -> UInt64 {
        lock.withLock {
            let handle = nextHandle
            nextHandle += 1
            tasks[handle] = Task.detached { await body(handle) }
            return handle
        }
    }

    /// Forget a finished stream's handle.
    static func finish(_ handle: UInt64) {
        lock.withLock { _ = tasks.removeValue(forKey: handle) }
    }

    /// Cancel a live stream; an unknown or finished handle is a no-op.
    static func cancel(_ handle: UInt64) {
        let task = lock.withLock { tasks[handle] }
        task?.cancel()
    }

    /// How many streams are live — for tests.
    static var live: Int { lock.withLock { tasks.count } }
}

/// The caller's pointers, carried into the stream task untouched.
private struct Callbacks: @unchecked Sendable {
    let ctx: UnsafeMutableRawPointer?
    let onChunk: FMChunkCallback
    let onDone: FMDoneCallback
}

/// A NUL-terminated heap copy the caller releases with `fm_free`.
private func heapCString(_ s: String) -> UnsafeMutablePointer<CChar>? {
    s.withCString { strdup($0) }
}

/// `{"available":bool,"reason":str|null}` on the heap — release with `fm_free`.
@_cdecl("fm_availability")
public func fm_availability() -> UnsafeMutablePointer<CChar>? {
    heapCString(FMBridge.engine.availability().json)
}

/// The on-device model's context window in tokens — read from the SDK; Apple's
/// documented 4096 where no SDK can be asked (where every stream is refused).
@_cdecl("fm_context_size")
public func fm_context_size() -> Int64 {
    Int64(FMBridge.engine.contextSize)
}

/// Start one generation. Returns its handle (>= 1), or 0 when a callback is
/// NULL — the only case in which nothing will be called back. Every failure
/// after that (a NULL or malformed `request_json`, an unavailable model, a
/// generation error) arrives through `on_done`.
@_cdecl("fm_stream")
public func fm_stream(
    _ requestJSON: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ onChunk: FMChunkCallback?,
    _ onDone: FMDoneCallback?
) -> UInt64 {
    guard let onChunk, let onDone else { return 0 }
    // COPIED NOW: the caller may free its buffer the moment this returns.
    let json: String? = requestJSON.map { String(cString: $0) }
    let cb = Callbacks(ctx: ctx, onChunk: onChunk, onDone: onDone)
    let engine = FMBridge.engine
    return FMBridge.start { handle in
        var promptTokens: Int64 = -1
        var failure: String? = nil
        do {
            guard let json else { throw FMError("request_json is NULL") }
            let request = try FMRequest(json: Data(json.utf8))
            for try await event in engine.stream(request) {
                switch event {
                case .text(var text):
                    text.withUTF8 { buf in cb.onChunk(cb.ctx, buf.baseAddress, UInt(buf.count)) }
                case .promptTokens(let n):
                    promptTokens = Int64(n)
                }
            }
            // A cancelled consumer's stream ENDS (it does not throw): that is
            // still a cancellation, never a short success.
            if Task.isCancelled { failure = FM_CANCELLED }
        } catch is CancellationError {
            failure = FM_CANCELLED
        } catch {
            failure = fmMessage(error)
        }
        FMBridge.finish(handle)
        if let failure {
            failure.withCString { cb.onDone(cb.ctx, $0, promptTokens) }
        } else {
            cb.onDone(cb.ctx, nil, promptTokens)
        }
    }
}

/// Cancel a live generation: its `on_done` follows with `"cancelled"`. A handle
/// that is unknown or already done is a no-op.
@_cdecl("fm_cancel")
public func fm_cancel(_ handle: UInt64) {
    FMBridge.cancel(handle)
}

/// Release a string the bridge allocated (`fm_availability`). NULL is a no-op.
@_cdecl("fm_free")
public func fm_free(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}
