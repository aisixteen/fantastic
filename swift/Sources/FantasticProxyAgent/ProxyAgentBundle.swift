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
//   shutdown   no host → {ok: true}
//              host    → forward
//   any other  no host → {error, reason: "no_host"}
//              host    → forward

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "proxy_agent.tools"

// ── Host protocol ──────────────────────────────────────────────────

/// Trait the embedding host implements. Swift classes / actors
/// conform to this and register via `registerHost`. All methods are
/// sync to match the UniFFI 0.29 callback-interface constraint that
/// the Apple app already relies on.
/// Trait the embedding host implements. Inherits from
/// `_ProxyAgentRegistrable` so any `ProxyAgentHost` works directly
/// with `Kernel.registerProxyAgent(agentId:host:)` — no explicit
/// bridging needed.
public protocol ProxyAgentHost: AnyObject, Sendable, _ProxyAgentRegistrable {}

/// Source-compatibility alias for the original UniFFI callback
/// interface name. Apps that imported `ProxyAgent` from
/// `FantasticKernelEmbedded` keep compiling unchanged.
public typealias ProxyAgent = ProxyAgentHost

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

public func clearHosts() {
    hostsLock.lock()
    defer { hostsLock.unlock() }
    hosts.removeAll()
}

// ── Bundle ─────────────────────────────────────────────────────────

// ── Kernel.registerProxyAgent / unregisterProxyAgent bridge ───────
//
// `FantasticKernel` declares `Kernel.registerProxyAgent(agentId:host:)`
// + `unregisterProxyAgent(agentId:)` (in `PublicAPI.swift`) that
// match the UniFFI surface. They route through these globals so
// `FantasticKernel` doesn't need to import `FantasticProxyAgent`
// (which would be a circular dep — proxy_agent already imports the
// kernel). Hook installed lazily on first ProxyAgentBundle init.
//
// Bridge converts the kernel-side `_ProxyAgentRegistrable`
// existential into the local `ProxyAgentHost` shape via a small
// wrapper class.

private final class HostAdapter: ProxyAgentHost {
    let inner: any _ProxyAgentRegistrable
    init(_ inner: any _ProxyAgentRegistrable) { self.inner = inner }
    func handle(payloadJson: String) -> String { inner.handle(payloadJson: payloadJson) }
    func onBoot() { inner.onBoot() }
    func onDelete() { inner.onDelete() }
}

private let installHookOnce: Void = {
    installProxyAgentRegistrationHook(
        register: { agentId, host in
            registerHost(agentId, HostAdapter(host))
        },
        unregister: { agentId in
            let had = hostFor(agentId) != nil
            unregisterHost(agentId)
            return had
        }
    )
}()

public struct ProxyAgentBundle: AgentBundle {
    public let name = "proxy_agent"

    public init() {
        // Wire Kernel.registerProxyAgent into our host registry the
        // first time anyone instantiates the bundle. dispatch-once
        // semantics via the file-private constant above.
        _ = installHookOnce
    }

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

        case ("shutdown", nil):
            return .object(["ok": .bool(true)])
        case ("shutdown", let h?):
            return forwardToHost(host: h, payload: payload)

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
            : "Host-implemented agent — no host registered yet. Verbs other than reflect/boot/shutdown will return {error, reason: \"no_host\"}."
        return .object([
            "id": .string(agentId.value),
            "sentence": .string(sentence),
            "kind": .string("proxy_agent"),
            "host_registered": .bool(hostRegistered),
            "verbs": [
                "reflect": "Identity + host_registered probe.",
                "boot": "Fire host.onBoot() if registered.",
                "shutdown": "Forward to host if registered.",
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
