// RunnerTransport — the seam every runner bundle implements.
//
// Mirrors Rust's `fantastic-runner-core::Transport` and the Python
// `runner_core` Transport seam. Both Swift runners share the *lifecycle
// dispatch* (which verbs exist, that `boot` returns `{ok:true}`, that
// `shutdown` drains + returns `{ok:true}`, the unknown-verb error). They
// differ entirely in *how each verb's work is carried out and what
// concrete reply it produces*:
//
// - local — subprocess of `fantastic` + tracked child processes + OS
//   signals; replies carry `pid` / a `children` list.
// - ssh — `ssh` exec + `ssh -L` tunnel; replies carry `local_port` /
//   `remote` / a tunnel `pid`.
//
// Because the reply *shapes* differ (and must stay byte-identical to the
// pre-refactor wire), the transport owns each verb's reply body.
// `RunnerCore` owns the dispatch skeleton that routes verbs to these
// methods.
//
// This module is pure cross-platform: NO `#if os(macOS)`. The macOS
// gating lives on the runner bundles that conform to this protocol.

import FantasticJSON
import FantasticKernel

/// One runner transport. Constructed per `handle` call by the runner
/// bundle (carrying whatever it needs: the agent id, a handle to its
/// process-state map). Methods are async because transport work is
/// I/O-bound (subprocess spawn, tunnel probe).
public protocol RunnerTransport: Sendable {
    /// `reflect` reply — identity + verb catalogue + live status.
    func reflect() async -> JSON

    /// `start` reply — bring the project up and report the resulting
    /// pid / port (local) or local_port / remote (ssh).
    func start() async -> JSON

    /// `stop` reply — tear the project down (idempotent).
    func stop() async -> JSON

    /// Drain ALL live work owned by this transport. Invoked by the
    /// shared `shutdown` verb (and by the bundle's `onShutdown`). The
    /// `shutdown` verb's `{ok:true}` reply is produced by the core, not
    /// here — this is a side-effecting drain only.
    func shutdownAll() async

    /// Handle a verb the shared skeleton does not own (local's `list`,
    /// ssh's `status`). Return `nil` to let the core emit the
    /// byte-identical `unknown verb` error.
    func handleVerb(_ verb: String) async -> JSON?
}

extension RunnerTransport {
    public func handleVerb(_ verb: String) async -> JSON? { nil }
}
