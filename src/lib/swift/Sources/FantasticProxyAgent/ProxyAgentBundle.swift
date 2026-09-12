// Host-implemented agent bundle.
//
// Mirrors Rust's `fantastic-proxy-agent::ProxyAgentBundle`. Every
// verb dispatched to a `proxy_agent.tools` agent forwards to a host
// (Swift class) keyed by the agent id. Primary use: SwiftUI views
// as first-class agents (chat_ui, header_ui, etc.), Apple FM via
// LanguageModelSession, future Vision / AppIntents / EventKit
// bundles.
//
// Wire shape parity with the Rust bundle:
//   reflect    no host → self-describes (host_registered: false)
//              host    → forward to host; overlay host_registered: true
//   boot       no host → {ok: true}
//              host    → host.onBoot() + forward
//   shutdown   lifecycle no-op → null (hosts tear down in onDelete)
//              host    → forward
//   any other  no host → {error, reason: "no_host"}
//              host    → forward

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "proxy_agent.tools"

// ── Host protocol ──────────────────────────────────────────────────

/// Trait the embedding host implements. Swift classes / actors conform to this
/// and register via `registerHost(_:_:)` — the ONE registration path.
///
/// `handle` is sync and takes/returns a JSON STRING: the host is foreign code,
/// so the boundary is a value both sides can build without sharing a type.
public protocol ProxyAgentHost: AnyObject, Sendable {
    func handle(payloadJson: String) -> String
    func onBoot()
    func onDelete()
}

extension ProxyAgentHost {
    public func onBoot() {}
    public func onDelete() {}
}

// ── Process-global registry ────────────────────────────────────────

/// Per-agent-id host map. Multiple proxy_agent instances in one
/// kernel each have their own entry. Lives as a process-global
/// because Swift classes can't be carried across UniFFI in the
/// real app; the same shape works for in-process Rust mock hosts.
private let hostsLock = NSLock()
nonisolated(unsafe) private var hosts: [AgentId: ProxyAgentHost] = [:]

public func registerHost(_ agentId: AgentId, _ host: ProxyAgentHost) {
    hostsLock.lock()
    defer { hostsLock.unlock() }
    hosts[agentId] = host
}

public func unregisterHost(_ agentId: AgentId) {
    hostsLock.lock()
    defer { hostsLock.unlock() }
    hosts.removeValue(forKey: agentId)
}

public func hostFor(_ agentId: AgentId) -> ProxyAgentHost? {
    hostsLock.lock()
    defer { hostsLock.unlock() }
    return hosts[agentId]
}

// ── Bundle ─────────────────────────────────────────────────────────

public struct ProxyAgentBundle: AgentBundle {
    public let name = "proxy_agent"

    public init() {}

    public var readme: String? {
        "proxy_agent — host-implemented agents. Register a Swift class via `registerHost(agentId, host)` after creating the agent; subsequent verbs flow through `host.handle(payloadJson:)`."
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        let host = hostFor(agentId)
        switch (verb, host) {
        case ("reflect", nil):
            return defaultReflect(agentId: agentId, hostRegistered: false)
        case ("reflect", let h?):
            return mergeReflect(agentId: agentId, host: h, payload: payload)

        case ("boot", nil):
            return .object([
                "ok": .bool(true),
                "host_registered": .bool(false),
            ])
        case ("boot", let h?):
            h.onBoot()
            return forwardToHost(host: h, payload: payload)

        case ("shutdown", _):
            return .null

        case (_, nil):
            return .object([
                "error":
                    .string("no host registered for proxy_agent \(agentId.value)"),
                "reason": .string("no_host"),
            ])
        case (_, let h?):
            return forwardToHost(host: h, payload: payload)
        }
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        if let h = hostFor(agentId) {
            h.onDelete()
        }
        unregisterHost(agentId)
    }

    // MARK: - Helpers

    private func defaultReflect(agentId: AgentId, hostRegistered: Bool) -> JSON {
        let sentence: String =
            hostRegistered
            ? "Host-implemented agent. See host_registered + verbs above for behaviour."
            : "Host-implemented agent — no host registered yet. Verbs other than reflect/boot will return {error, reason: \"no_host\"}."
        return .object([
            "id": .string(agentId.value),
            "sentence": .string(sentence),
            "kind": .string("proxy_agent"),
            "host_registered": .bool(hostRegistered),
            "verbs": [
                "reflect": "Identity + host_registered probe.",
                "boot": "Fire host.onBoot() if registered.",
                "*": "Any other verb forwards to host.handle.",
            ] as JSON,
        ])
    }

    private func forwardToHost(host: ProxyAgentHost, payload: JSON) -> JSON {
        let payloadStr = payload.serialize()
        let replyStr = host.handle(payloadJson: payloadStr)
        if let parsed = try? JSON.parse(replyStr) {
            return parsed
        }
        return .object([
            "error": .string("proxy_agent host returned non-JSON"),
            "reason": .string("host_reply_malformed"),
            "reply_raw": .string(replyStr),
        ])
    }

    private func mergeReflect(agentId: AgentId, host: ProxyAgentHost, payload: JSON) -> JSON
    {
        let hostReply = forwardToHost(host: host, payload: payload)
        guard case var .object(dict) = hostReply else {
            // Host returned non-object — surface alongside synthetic reflect.
            var fallback = defaultReflect(agentId: agentId, hostRegistered: true)
            if case var .object(m) = fallback {
                m["host_reply"] = hostReply
                fallback = .object(m)
            }
            return fallback
        }
        dict["host_registered"] = .bool(true)
        if dict["id"] == nil {
            dict["id"] = .string(agentId.value)
        }
        if dict["kind"] == nil {
            dict["kind"] = .string("proxy_agent")
        }
        return .object(dict)
    }
}
