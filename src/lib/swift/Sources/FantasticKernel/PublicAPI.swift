// The Swift kernel's public surface: what an EMBEDDING APP calls beyond `send`.
//
// `KernelStartupError` (what the startup entry points throw) and the HTTP port
// accessors. Snapshots are `saveJSON`/`loadJSON` (`KernelState.swift`); the
// graceful `shutdown()` lives in `Lifecycle.swift`.

import FantasticJSON
import Foundation

// MARK: - Startup error

/// What `startKernel` / `Bootstrap.daemon` throw before a kernel exists.
public enum KernelStartupError: Error, Sendable {
    case workdirInvalid(String)
    case internalError(String)
}

// MARK: - HTTP port accessor

extension Kernel {
    /// The port the HTTP listener is bound to, or 0 if no listener is running.
    public func httpPort() -> UInt16 {
        return httpPortValue
    }

    /// Set when the listener binds (0 when it stops). Public so the
    /// `FantasticWeb` target can write it.
    public func setHttpPort(_ port: UInt16) {
        httpPortLock.lock()
        defer { httpPortLock.unlock() }
        _httpPort = port
    }

    fileprivate var httpPortValue: UInt16 {
        httpPortLock.lock()
        defer { httpPortLock.unlock() }
        return _httpPort
    }
}
