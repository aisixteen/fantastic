// RunnerCore — the shared lifecycle verb dispatcher.
//
// This is the deduplicated body of both runner bundles' `handle`. It
// takes a fully-built `RunnerTransport` (constructed per call by the
// runner bundle) and routes the lifecycle verb to it:
//
// - `reflect` / `start` / `stop` → the transport's matching method
//   (which owns the concrete reply body).
// - `boot` → `{ok:true}` (no auto-start; `start` is explicit).
// - `shutdown` → transport drains all work, then `{ok:true}`.
// - any other verb → the transport's `handleVerb` (local's `list`,
//   ssh's `status`); `nil` from there falls through to the unknown-verb
//   error.
// - unknown verb → `{"error": "unknown verb <verb>"}`.
//
// No wire/verb/event behaviour changes here vs. the pre-refactor
// per-runner `handle`.

import FantasticJSON
import FantasticKernel

/// Stateless dispatcher over a `RunnerTransport`. Mirrors Rust's
/// `fantastic-runner-core::RunnerCore`.
public enum RunnerCore {
    /// Dispatch one lifecycle verb through `transport`. `verb` is the
    /// payload's `type`.
    public static func handle(verb: String, transport: some RunnerTransport) async -> JSON {
        switch verb {
        case "reflect":
            return await transport.reflect()
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            await transport.shutdownAll()
            return .object(["ok": .bool(true)])
        case "start":
            return await transport.start()
        case "stop":
            return await transport.stop()
        default:
            if let reply = await transport.handleVerb(verb) {
                return reply
            }
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
