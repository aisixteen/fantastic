// The AI layer's TWO event primitives — where an event goes, and what a
// `status` event carries.
//
// Ported from python `ai_core/core.py` (`_to_caller`, `_emit_status`,
// core.py:1622-1676). They live in their own file because EVERY emitting site
// goes through them: the token stream and the phase events (AIBackend.swift),
// both context events (ContextProjection.swift), and the delivery / await
// announcements (Routing.swift). Four spellings of "emit an event" is how the
// runtimes drifted — one of them put `reason` at the top level, another carried
// no `client_id`, and a third auto-filled nothing.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Route ONE stream event to the originating caller ONLY. Python `_to_caller`.
///
/// ONE ROUTE, decided by the client id:
///   - `cli` → a `send` to the `cli` agent, whose handler prints it (the REPL and
///     headless flow);
///   - anything else (a browser tab's id, a harness's) → an `emit` on this
///     backend's own inbox, which that caller's WS subscription mirrors and
///     filters to its own id.
///
/// No fan-out: the cli never sees a browser's tokens, and a browser never sees
/// the cli's. Emitting EVERYTHING on the inbox would leave a cli-driven turn printing
/// nothing unless something happened to be watching the agent. The event is
/// stamped with `client_id` LAST (python's `{**ev, "client_id": …}`).
func toCaller(
    kernel: Kernel,
    agentId: AgentId,
    clientId: String,
    event: OrderedDictionary<String, JSON>
) async {
    var ev = event
    ev["client_id"] = .string(clientId)
    if clientId == AIConstants.DEFAULT_CLIENT_ID {
        // The outcome is not read, as in python: a missing or ungranted `cli`
        // agent refuses the send, and the stream carries on.
        _ = await kernel.send([Item(AgentId("cli"), .object(ev))])
    } else {
        await kernel.emit(agentId, .object(ev))
    }
}

/// Broadcast a structured `status` event AND keep the in-flight entry's phase in
/// sync, so the on-demand `status` verb agrees with the last event. Python
/// `_emit_status`.
///
/// `send_id` and `started_at` are pulled from the agent's current entry so no
/// call site threads them through, and `queue_depth` from its queue. A caller
/// that owns the fields (the `queued` probe, `on_cancel` after it took the
/// entry) passes them in `detail` and they stand.
func emitStatus(
    kernel: Kernel,
    agentId: AgentId,
    clientId: String,
    phase: String,
    detail: [(String, JSON)] = []
) async {
    let st = AIRunStates.of(kernel, agentId.value)
    st.updateCurrent { $0.phase = phase }
    var d: OrderedDictionary<String, JSON> = [:]
    for (k, v) in detail { d[k] = v }
    if let cur = st.currentSnapshot() {
        if d["send_id"] == nil { d["send_id"] = .string(cur.sendId) }
        if d["started_at"] == nil { d["started_at"] = .double(cur.startedAt) }
    }
    if d["queue_depth"] == nil { d["queue_depth"] = .integer(Int64(st.queueDepth())) }
    await toCaller(
        kernel: kernel, agentId: agentId, clientId: clientId,
        event: [
            "type": .string("status"),
            "source": .string(agentId.value),
            "phase": .string(phase),
            "detail": .object(d),
            "ts": .double(Date().timeIntervalSince1970),
        ])
}

/// The per-agent send coordination state, shared by every backend AND by the
/// router — python's `_queue` / `_current` are module dicts, which in a
/// one-kernel-per-process runtime means exactly "per kernel, per agent".
///
/// It was per-`AIBackend`, which is per BUNDLE: the router, which holds no
/// bundle, could not read the target's in-flight entry at all, so an announced
/// delivery failure carried neither the send it belonged to nor the depth of the
/// queue behind it.
///
/// KEYED BY KERNEL, not process-wide: swift hosts several kernels in one process
/// (every test does), and a flat id→state map made two kernels with an agent of
/// the same name share one FIFO lock and one queue.
enum AIRunStates {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var states: [UInt64: [String: AIAgentRunState]] =
        [:]

    /// The state for an agent of THIS kernel.
    static func of(_ kernel: Kernel, _ id: String) -> AIAgentRunState {
        lock.lock()
        defer { lock.unlock() }
        let key = kernel.serial
        if let s = states[key]?[id] { return s }
        let s = AIAgentRunState()
        states[key, default: [:]][id] = s
        return s
    }

    /// Drop a dead kernel's whole sub-table — see `AISideTables`.
    static func forget(_ kernel: Kernel) {
        lock.lock()
        states.removeValue(forKey: kernel.serial)
        lock.unlock()
    }

    /// Drop a deleted agent's state — its FIFO lock, queue and in-flight entry
    /// (`AIBackend.onDelete`).
    static func forget(_ kernel: Kernel, _ id: String) {
        lock.lock()
        states[kernel.serial]?.removeValue(forKey: id)
        lock.unlock()
    }
}
