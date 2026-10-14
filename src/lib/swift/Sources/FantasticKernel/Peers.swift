// CONNECTED PEER KERNELS — cross-kernel addressing. Python
// `PeerRoute` + `Kernel.peers` / `register_peer` / `unregister_peer` /
// `peer_rows` (`kernel/_kernel.py`) and `RemoteTarget` / `_qualified_target`
// (`kernel/_agent.py`).
//
// An agent on another kernel is addressed `<guid>|<agent id>` and called with an
// ordinary `send`. When that guid is registered here, the send hands the call to
// its routes and the peer's reply is the call's ordinary outcome. A TRANSPORT
// registers the peer it is connected to (the web ws leg, when a page says
// `hello`) and unregisters it when the connection ends — the transport only
// carries frames; addressing is the kernel's. A guid that is not here is an
// unknown target like any other.
//
// A SET PER GUID: the tabs of one identity are ONE principal
// — every tab says hello with the identity's guid, each is a live route of it, a
// call to `<guid>|x` goes to all of them and settles with the first reply.

import FantasticJSON
import Foundation
import OrderedCollections

/// How a connected PEER KERNEL is reached. `via` names the transport agent
/// carrying it (the root reflect reports it); `call(agentId, payload, body)`
/// delivers ONE call to that kernel's `agentId` and returns the peer's reply —
/// or a refusal when the connection ends first. The peer judges what arrives as
/// the transport leg that serves it there — a caller is always local. Python
/// `PeerRoute.call(agent_id, payload)`.
///
/// A CLASS, compared by IDENTITY (python `is`): a connection unregisters its own
/// route and no other. The request body rides beside the JSON
/// at its payload path (`Blob`), as it does everywhere in this runtime — python's
/// payload dict simply holds the bytes there; the reply's chunk rides beside the
/// reply at ITS slot of the reply (`Blob`), where the peer's frame put it.
///
/// `emit(agentId, payload, body)` delivers ONE event to that kernel's `agentId`.
/// SYNCHRONOUS like the kernel's own emit (a fanout, no reply), so a transport
/// puts emits on its wire in the order they were made.
public final class PeerRoute: @unchecked Sendable {
    /// The transport agent carrying this peer.
    public let via: String
    private let deliver: @Sendable (String, JSON, Blob?) async -> (JSON, Blob?)
    private let deliverEmit: @Sendable (String, JSON, Blob?) -> Void

    public init(
        via: String,
        call: @escaping @Sendable (String, JSON, Blob?) async -> (JSON, Blob?),
        emit: @escaping @Sendable (String, JSON, Blob?) -> Void
    ) {
        self.via = via
        self.deliver = call
        self.deliverEmit = emit
    }

    /// Deliver one event through this route.
    public func emit(_ agentId: String, _ payload: JSON, body: Blob?) {
        deliverEmit(agentId, payload, body)
    }

    /// Deliver one call through this route.
    public func call(_ agentId: String, _ payload: JSON, body: Blob?) async -> (JSON, Blob?) {
        await deliver(agentId, payload, body)
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
    /// The guid's live routes when the send resolved it — every tab of one
    /// identity: the call goes to ALL of them and settles with the first reply.
    let routes: [PeerRoute]
}

extension Kernel {
    /// Make `<guid>|<agent>` reachable through `route` — ADDED to the guid's live
    /// routes: another connection saying hello with a live guid is another tab of
    /// that identity, one principal with the rest. A route already there stays
    /// one.
    public func registerPeer(_ guid: String, _ route: PeerRoute) {
        peersLock.lock()
        defer { peersLock.unlock() }
        var routes = _peers[guid] ?? []
        if !routes.contains(where: { $0 === route }) { routes.append(route) }
        _peers[guid] = routes
    }

    /// Forget `route` — that one connection. The guid stays reachable through its
    /// other live routes, and stops being an address with its last.
    public func unregisterPeer(_ guid: String, _ route: PeerRoute) {
        peersLock.lock()
        defer { peersLock.unlock() }
        let rest = (_peers[guid] ?? []).filter { $0 !== route }
        _peers[guid] = rest.isEmpty ? nil : rest
    }

    /// The live routes `guid` is connected through — empty when it is not
    /// connected.
    public func peerRoutes(_ guid: String) -> [PeerRoute] {
        peersLock.lock()
        defer { peersLock.unlock() }
        return _peers[guid] ?? []
    }

    /// The connected peer kernels, for the root reflect: `[{guid, via}]`, in
    /// python's `sorted(self.peers.items())` order — one row per live route, so a
    /// guid connected twice shows twice.
    public func peerRows() -> JSON {
        peersLock.lock()
        let table = _peers.sorted { $0.key < $1.key }
        peersLock.unlock()
        return .array(
            table.flatMap { g, routes in
                routes.map { .object(["guid": .string(g), "via": .string($0.via)]) }
            })
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
        let routes = peerRoutes(guid)
        if routes.isEmpty { return nil }
        return Agent(
            remote: AgentId(targetId), RemoteRef(guid: guid, agentId: agentId, routes: routes))
    }

    /// The dispatch of a call to a remote target: hand it to EVERY live route of
    /// the peer (each tab of one identity); the first reply settles it and the
    /// other calls are cancelled. Python `RemoteTarget._dispatch_task`.
    func callPeer(_ remote: RemoteRef, qualified: AgentId, payload: JSON, body: Blob?) async
        -> (JSON, Blob?)
    {
        await withTaskGroup(of: (JSON, Blob?).self) { group in
            for route in remote.routes {
                group.addTask { await route.call(remote.agentId, payload, body: body) }
            }
            // TODO: first-reply-wins hides tab DESYNC — if the tabs
            // of one identity diverge, the other replies are dropped unseen. Must
            // be solved (later).
            let first = await group.next()!
            group.cancelAll()  // the losers — their routes take their calls back
            return first
        }
    }

    /// The dispatch of an EMIT to a remote target: the event goes to EVERY live
    /// route of the peer (each tab of one identity).
    ///
    /// TODO: DESYNC between the tabs of one identity is envisioned
    /// and unsolved — each tab applies the event on its own; think later.
    func emitPeer(_ remote: RemoteRef, item: InboxItem) {
        for route in remote.routes {
            route.emit(remote.agentId, item.payload, body: item.body)
        }
    }
}
