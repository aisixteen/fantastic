// Storage tier — disk vs in-memory.
//
// Mirrors Rust's `fantastic_kernel::StorageMode`. Disk mode persists
// each agent's `agent.json` under a workdir; InMemory mode skips
// all disk writes (per-agent readme seeding, persistence, save/load
// to file become no-ops).

import Foundation

public enum StorageMode: Sendable, Equatable {
    /// Persistent — each agent's record lives in
    /// `<workdir>/.fantastic/agents/<id>/agent.json`.
    case disk(URL)
    /// In-memory only. Used by app-embedded kernels (brain) and
    /// most tests.
    case inMemory

    public var isDisk: Bool {
        if case .disk = self { return true }
        return false
    }

    public var isInMemory: Bool {
        if case .inMemory = self { return true }
        return false
    }

    public var workdir: URL? {
        if case .disk(let url) = self { return url }
        return nil
    }

    /// Path of the canonical state file (`.fantastic/state.json`)
    /// in Disk mode. Used by `kernel.save()` / `kernel.load()` for
    /// snapshot round-trips.
    public var stateFile: URL? {
        workdir?.appendingPathComponent(".fantastic/state.json")
    }
}
