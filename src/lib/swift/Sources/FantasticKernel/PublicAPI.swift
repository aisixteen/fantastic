// The Swift kernel's public surface: what an EMBEDDING APP calls.
//
// This was a UniFFI-compatibility shim layer — JSON-string methods mirroring
// `fantastic-uniffi` so an Apple app's call sites survived the XCFramework →
// native Swift migration. The migration is done and the app is gone, so the
// dead half is DELETED: `sendJson`/`sendJsonAs`/`proxyEmit`, the
// `registerTool`/`listToolsForLlm`/`dispatchTool` registry, and the
// `StateListener` callback bridge. Nothing referenced them but their own tests.
//
// The tool registry went with them for a second reason: a
// register-tools-then-list-them-for-the-LLM surface IS the native tool API, and
// tool-calling here is RAW prompt-and-parse owned by the agent base class.
// Keeping an unused one invites a future caller to wire it back.
//
// What remains is what callers actually use: `subscribe`, the HTTP port
// accessors, and `save`/`load`. The graceful `shutdown()` lives in
// `Lifecycle.swift`.

import FantasticJSON
import Foundation

// MARK: - Kernel error (matches UniFFI's KernelError variants)

public enum KernelStartupError: Error, Sendable {
    case workdirInvalid(String)
    case portBindFailed(String)
    case bootFailed(String)
    case alreadyRunning
    case invalidSnapshot(String)
    case internalError(String)
}

// MARK: - HTTP port accessor (real binding lands in 8B)

extension Kernel {
    /// The port the HTTP listener is bound to, or 0 if no listener
    /// is running. Real implementation lands with the Hummingbird
    /// HTTP listener in phase 8B; pre-listener kernels report 0.
    public func httpPort() -> UInt16 {
        return httpPortValue
    }

    /// Phase 8B sets this when the listener binds. Public so the
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

// MARK: - save/load JSON-string shims

extension Kernel {
    /// Snapshot kernel state as a JSON string. Matches Rust UniFFI's
    /// `save() -> String` which returns the same shape.
    public func save() -> String {
        do {
            return try saveJSON()
        } catch {
            return JSON.object([
                "error": .string("save failed: \(error)")
            ]).serialize()
        }
    }

    /// Restore agent tree from JSON. Throws on malformed snapshot
    /// (version, duplicates, dangling parents, no root).
    public func load(json: String) throws {
        try loadJSON(json)
    }
}
