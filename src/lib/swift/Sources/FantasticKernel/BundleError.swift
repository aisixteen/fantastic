// Errors a bundle can surface to the kernel.
//
// Rust uses `Box<dyn std::error::Error + Send + Sync>` for
// open-ended bundle errors. Swift maps this to a public protocol
// that bundles can conform their own error types to; the kernel
// catches and converts to `{"error": "..."}` JSON replies.
//
// We also ship a concrete `KernelError` for substrate-level
// failures (missing agent, no handler_module, bundle not registered)
// — these match the JSON shapes the Rust kernel emits today.

import Foundation

/// Marker protocol for errors a bundle's `handle` / `onDelete` /
/// `onShutdown` can throw. Any `Swift.Error` works — this just
/// names the contract.
public typealias BundleError = Swift.Error

/// Substrate-level kernel errors. These are the conditions the
/// kernel surfaces as structured JSON replies on the wire.
public enum KernelError: Error, Equatable, Sendable {
    /// `kernel.send(target_id, ...)` resolved to no registered agent.
    case noAgent(AgentId)
    /// Target agent has no `handler_module` set + the verb isn't a
    /// substrate-native one (`reflect`, `create_agent`, etc.).
    case noHandlerModule(AgentId, verb: String)
    /// Agent's `handler_module` doesn't match any registered bundle.
    case noBundleForHandlerModule(String)
    /// Caller-supplied JSON payload was missing a required field or
    /// had a wrong type for an existing field.
    case invalidPayload(String)
    /// Snapshot supplied to `Kernel.load` was malformed (missing
    /// root / duplicate id / dangling parent / unknown version).
    case invalidSnapshot(String)
    /// Lock file owned by a still-live process at `pid`.
    case alreadyRunning(pid: Int32, workdir: String)
}

extension KernelError {
    /// String form of the error suitable for emitting in the
    /// kernel's `{"error": "..."}` JSON reply. Matches the Rust
    /// kernel's `Display` impl for parity.
    public var wireMessage: String {
        switch self {
        case .noAgent(let id):
            return "no agent \(id)"
        case .noHandlerModule(let id, let verb):
            return "agent \(id) has no handler_module; cannot answer verb \(verb)"
        case .noBundleForHandlerModule(let module):
            return "no bundle for handler_module \"\(module)\""
        case .invalidPayload(let detail):
            return "invalid payload: \(detail)"
        case .invalidSnapshot(let detail):
            return "invalid snapshot: \(detail)"
        case .alreadyRunning(let pid, let workdir):
            return "already running (pid \(pid)) in \(workdir)"
        }
    }
}
