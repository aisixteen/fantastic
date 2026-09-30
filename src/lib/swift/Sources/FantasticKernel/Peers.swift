// CONNECTED PEER KERNELS — cross-kernel addressing (owner, 2026-09-30). Python
// `PeerRoute` + `Kernel.peers` / `register_peer` / `unregister_peer` /
// `peer_rows` (`kernel/_kernel.py`) and `RemoteTarget` / `_qualified_target`
// (`kernel/_agent.py`).
//
// An agent on another kernel is addressed `<guid>|<agent id>` and called with an
// ordinary `send`. When that guid is registered here, the send hands the call to
// its route and the peer's reply is the call's ordinary outcome. A TRANSPORT
// registers the peer it is connected to (the web ws leg, when a page says
// `hello`) and unregisters it when the connection ends — the transport only
// carries frames; addressing is the kernel's. A guid that is not here is an
// unknown target like any other.

import FantasticJSON
import Foundation
import OrderedCollections

/// How a connected PEER KERNEL is reached. `via` names the transport agent
/// carrying it (the root reflect reports it); `call(agentId, payload, caller,
/// body)` delivers ONE call to that kernel's `agentId` as `caller` (`<this
/// guid>|<sender>`) and returns the peer's reply — or a refusal when the
/// connection ends first.
///
/// A CLASS, compared by IDENTITY (python `is`): only the connection that
/// registered a guid can unregister it. The body rides beside the JSON, as it
/// does everywhere in this runtime — python's reply dict simply holds the bytes.
public final class PeerRoute: @unchecked Sendable {
    /// The transport agent carrying this peer.
    public let via: String
    private let deliver: @Sendable (String, JSON, String, Data?) async -> (JSON, Data?)

    public init(
        via: String,
        call: @escaping @Sendable (String, JSON, String, Data?) async -> (JSON, Data?)
    ) {
        self.via = via
        self.deliver = call
    }

    /// Deliver one call through this route.
    public func call(
        _ agentId: String, _ payload: JSON, caller: String, body: Data?
    ) async -> (JSON, Data?) {
        await deliver(agentId, payload, caller, body)
    }
}

/// An agent on a CONNECTED PEER KERNEL, as ONE send sees it — python's
/// `RemoteTarget`. Carried on an unregistered `Agent` so the send pipeline treats
/// it like any target (a ledger row, a task-table row, a settled outcome), except
/// that its dispatch hands the call to the peer's route instead of a handler here.
struct RemoteRef: Sendable {
    /// The peer kernel's guid.
    let guid: String
    /// The agent's id on that kernel.
    let agentId: String
    /// The route the send hands the call to.
    let route: PeerRoute
}

extension Kernel {
    /// Make `<guid>|<agent>` reachable through `route`. Refused (the refusal
    /// returned) when that guid is already connected through ANOTHER route —
    /// re-pointing a live address silently would hand one kernel's calls to
    /// another; the second connection is told so instead.
    public func registerPeer(_ guid: String, _ route: PeerRoute) -> JSON? {
        peersLock.lock()
        defer { peersLock.unlock() }
        if let held = _peers[guid], held !== route {
            return Refusal.make(
                Refusal.badArg,
                "peer \(guid) is already connected via \(held.via) — one kernel, one live "
                    + "route; close the other connection first",
                extra: [("arg", .string("guid"))])
        }
        _peers[guid] = route
        return nil
    }

    /// Forget `guid` — only if `route` is still the one registered.
    public func unregisterPeer(_ guid: String, _ route: PeerRoute) {
        peersLock.lock()
        defer { peersLock.unlock() }
        if _peers[guid] === route { _peers[guid] = nil }
    }

    /// The route `guid` is connected through, if it is connected.
    public func peer(_ guid: String) -> PeerRoute? {
        peersLock.lock()
        defer { peersLock.unlock() }
        return _peers[guid]
    }

    /// The connected peer kernels, for the root reflect: `[{guid, via}]`, in
    /// python's `sorted(self.peers.items())` order.
    public func peerRows() -> JSON {
        peersLock.lock()
        let rows = _peers.sorted { $0.key < $1.key }
        peersLock.unlock()
        return .array(
            rows.map { .object(["guid": .string($0.key), "via": .string($0.value.via)]) })
    }

    /// This kernel's own guid — the root record's `guid`, once its loader minted
    /// it. `nil` before the root has booted.
    func ownGuid() -> String? {
        guard let g = root?.metaValue(forKey: "guid")?.asString, !g.isEmpty else { return nil }
        return g
    }

    /// `<guid>|<agent id>`: this kernel's own guid names a LOCAL agent; the guid
    /// of a CONNECTED PEER names an agent there (a remote target the send hands
    /// to that peer's route); anything else names nothing. Python
    /// `Agent._qualified_target`.
    func qualifiedTarget(_ targetId: String) -> Agent? {
        guard let bar = targetId.firstIndex(of: "|") else { return nil }
        let guid = String(targetId[..<bar])
        let agentId = String(targetId[targetId.index(after: bar)...])
        if guid.isEmpty || agentId.isEmpty || agentId.contains("|") { return nil }
        if guid == ownGuid() {
            return agentId == "kernel" ? root : agent(AgentId(agentId))
        }
        guard let route = peer(guid) else { return nil }
        return Agent(
            remote: AgentId(targetId), RemoteRef(guid: guid, agentId: agentId, route: route))
    }

    /// The dispatch of a call to a remote target: hand it to the peer's route,
    /// carrying the caller as `<this guid>|<sender>` so the peer's gate can judge
    /// it. Python `RemoteTarget._dispatch_task`.
    func callPeer(
        _ remote: RemoteRef, qualified: AgentId, payload: JSON, origin: AgentId?, body: Data?
    ) async -> (JSON, Data?) {
        guard let own = ownGuid() else {
            // The peer judges the caller by `<guid>|<agent>`; with no guid of our
            // own there is nothing honest to put on the frame.
            return (
                Refusal.make(
                    Refusal.unhandled,
                    "send: this kernel has no guid yet (its root has not booted), so a "
                        + "call to \(qualified.value) cannot say who is calling"),
                nil
            )
        }
        return await remote.route.call(
            remote.agentId, payload, caller: "\(own)|\(origin?.value ?? "kernel")", body: body)
    }
}
