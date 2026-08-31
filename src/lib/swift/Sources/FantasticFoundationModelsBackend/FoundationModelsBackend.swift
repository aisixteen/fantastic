// Apple Foundation Models LLM backend — kernel-native bundle.
//
// Wraps Apple's `FoundationModels` framework (`LanguageModelSession`)
// behind the same verb surface as the ollama + NIM backends, so chat
// UIs speak to Apple FM exactly the way they speak to any other LLM
// backend in the kernel.
//
// Design: ATOMIC + STATELESS. Every `send` builds a fresh
// `LanguageModelSession(instructions:tools:)`, feeds it ONE user
// message, streams the reply, lets ARC drop the session. No
// transcript carries between calls. The 3B on-device model has a
// 4096-token window — treating each call as `(system + tools +
// input) → reply` keeps the full budget available every time and
// architecturally avoids the long-context drift small models hit
// inside multi-turn chats. History (in-memory per `(agent_id,
// client_id)`, owned by the shared `AIBackend`) is kept ONLY so the
// `history` verb can return the UI bubble log — it is NEVER fed back
// to the model (the shared core runs in `stateless` mode for FM).
//
// The model's DURABLE memory is the always-inject path: at `boot` the
// bundle self-mounts `mem` + `data` durable_memory agents, and
// `makeProvider` splices each agent's current keys into the
// session instructions on EVERY send. Recall is structural, not
// prompted.
//
// All the shared agent machinery (history, epoch cancellation, verb
// dispatch, token/done events) lives in `FantasticAICore.AIBackend`.
// This file is the ONLY place in the whole package that imports
// FoundationModels — all `#if canImport` / `#available` gating is
// isolated here; the shared core stays provider-agnostic.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

#if canImport(FoundationModels)
    import FoundationModels
#endif

public let HANDLER_MODULE = "foundation_models_backend.tools"

public final class FoundationModelsBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = "foundation_models_backend"

    private let core: AIBackend

    public init() {
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: "foundation_models_backend",
                provider: "apple_foundation_models",
                sentence:
                    "Apple Foundation Models LLM agent (on-device; raw prompt-and-parse tool-calling).",
                verbs: [
                    "send":
                        "args: text, client_id?. Atomic stateless call: (system + tools + text) → streaming reply.",
                    "history":
                        "args: client_id?. Returns UI bubble log (not fed back to the model).",
                    "cancel": .string(CANCEL_DOC),
                    "status":
                        "args: client_id?. In-flight phase + this client's pending queue (others' text redacted).",
                    "backend_state": "Reports availability + in-flight count.",
                ] as JSON,
                // FM: stateless context (the on-device session owns it).
                stateless: true,
                reflectExtra: { _ in
                    [
                        "available": .bool(Self.isAvailable()),
                        "model": .string(Self.modelLabel()),
                    ]
                },
                backendStateExtra: { [weak coreBox] _ in
                    [
                        "apple_intelligence_available": .bool(Self.isAvailable()),
                        "model_available": .bool(Self.isAvailable()),
                        "backend_registered": .bool(true),
                        "model": .string(Self.modelLabel()),
                        "in_flight": .integer(Int64(coreBox?.value?.readInFlight() ?? 0)),
                        "reason": .string(Self.isAvailable() ? "ok" : Self.unavailableReason()),
                    ]
                },
                // Always-inject durable memory: the shared core appends
                // this AFTER the substrate prompt (primer + menu + send
                // how-to). Each mounted durable_memory agent's keys +
                // any custom `instructions` meta land in the FM session's
                // instructions every turn — recall is structural.
                systemPromptExtra: { agent, kernel in
                    await Self.fullInstructions(agent: agent, kernel: kernel)
                },
                makeProvider: { agent, _, kernel in
                    guard Self.isAvailable() else {
                        return .refused(
                            .object([
                                "error": .string("foundation_models_unavailable"),
                                "reason": .string(Self.unavailableReason()),
                            ]))
                    }
                    // Instructions now flow through the assembled system
                    // message (extracted by the provider from `messages`);
                    // the provider needs only temperature + the kernel for
                    // its native universal `send` tool.
                    let temp = Self.temperature(agent: agent)
                    return .provider(
                        FoundationModelsProvider(temperature: temp, kernel: kernel))
                }
            ))
        coreBox.value = core
    }

    /// Lets the `backendStateExtra` closure read the shared core's live
    /// in-flight count without capturing `self` (which doesn't exist
    /// yet when the closure is built inside `init`).
    private let coreBox = WeakBox()

    public var readme: String? {
        """
        foundation_models_backend — Apple on-device Foundation Models LLM backend; thin over FantasticAICore.
        Verbs: send/history/interrupt/backend_state. Same LLM backend contract as \
        ollama_backend; runs stateless against the native on-device model.
        Tool-calling is RAW: no Apple @Generable Tool — the model generates text and \
        FantasticAICore parses the <tool_call>/<tool_response> envelope (tool_parse).
        Durable memory: on boot, mem+data durable_memory agents are auto-mounted and their \
        contents spliced into the model instructions on every send (always-on; no explicit recall needed).
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        // `boot` self-mounts memory agents, then delegates to the shared
        // core (which also answers `boot` with `{ok:true}`).
        if payload["type"].asString == "boot" {
            await mountMemoryAgents(agentId: agentId, kernel: kernel)
        }
        return await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent. If it is the in-flight
    /// generation, close the turn out for the caller before the substrate cancels
    /// it (which tears down the on-device session stream on its own). There is no
    /// `interrupt` verb any more: cancellation is the kernel's.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    // MARK: - Durable memory (self-mount + always-inject)

    /// Idempotently mount `mem` + `data` durable_memory agents under
    /// this backend at boot, so the model's durable memory exists — and
    /// is auto-injected — before turn one. No-op if an agent of each mode
    /// already exists (children rehydrate from disk on reboot).
    func mountMemoryAgents(agentId: AgentId, kernel: Kernel) async {
        let have = Set(
            memoryAgents(agentId: agentId, kernel: kernel).compactMap {
                kernel.agent($0)?.metaValue(forKey: "mode")?.asString
            })
        // durable_memory persists THROUGH a file_bridge provider (no own disk
        // surface). Discover the `.fantastic` store + wire `file_bridge_id` so
        // the mounted memory persists. No store wired ⇒ field omitted (the
        // memory is RAM-only and `set` failfasts — no silent fallback).
        let storeId = kernel.findStore()?.value

        // Self-wire THIS backend's own `file_bridge_id` to the same store
        // so the shared core persists chat history through it (FM is
        // self-bootstrapping — it already self-mounts memory + discovers
        // the store; ollama/NIM expect the operator to set this). Only when
        // absent + a store exists; no store ⇒ history stays RAM-empty.
        if let storeId, kernel.agent(agentId)?.metaValue(forKey: "file_bridge_id") == nil {
            _ = await kernel.send([Item(agentId, .object([
                    "type": .string("update_agent"),
                    "id": .string(agentId.value),
                    "file_bridge_id": .string(storeId),
                ]))])[0].result
        }

        for mode in ["mem", "data"] where !have.contains(mode) {
            var rec: JSON = .object([
                "type": .string("create_agent"),
                // ITS OWN subtree: placement is stated by `parent_id`, and the
                // kernel-owned verb goes to the root.
                "parent_id": .string(agentId.value),
                "handler_module": .string("durable_memory.tools"),
                "mode": .string(mode),
            ])
            if let storeId {
                rec["file_bridge_id"] = .string(storeId)
                // Two-sided deny-all: the mounted memory persists read/write/delete
                // to the store directly, so wiring `file_bridge_id` also grants that
                // egress edge on the child.
                rec["can_call"] = .object([storeId: .string("*")])
            }
            // Boot-time self-provisioning runs FROM ABOVE (operator): the backend
            // creates its OWN memory subtree, confers the store egress its children
            // need (a subset it discovered, not one it holds itself), AND opens the
            // store's reciprocal `callable_by` for each child — exactly the authority
            // an operator has, which a lateral agent send does not.
            let reply = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([Item(AgentId("kernel"), rec)])[0].result
            }
            if let storeId, let childId = reply["id"].asString {
                await openStoreIngress(store: storeId, caller: childId, kernel: kernel)
            }
        }
    }

    /// Open the store's `callable_by` for a mounted memory child (ingress half of
    /// the two-sided gate), merged with the store's existing grant. Runs as operator
    /// — the boot-time provisioning authority.
    private func openStoreIngress(store: String, caller: String, kernel: Kernel) async {
        var accept: JSON =
            kernel.agent(AgentId(store))?.metaValue(forKey: "callable_by") ?? .object([:])
        if case .object(var map) = accept {
            map[caller] = .string("*")
            accept = .object(map)
        }
        _ = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("update_agent"),
                    "id": .string(store),
                    "callable_by": accept,
                ]))])[0].result
        }
    }

    /// Agent ids of this backend's mounted durable_memory agents.
    func memoryAgents(agentId: AgentId, kernel: Kernel) -> [AgentId] {
        guard let agent = kernel.agent(agentId) else { return [] }
        return agent.childIds().filter {
            kernel.agent($0)?.handlerModule == "durable_memory.tools"
        }
    }

    /// The system-prompt body + each mounted durable_memory agent's
    /// current keys spliced in. Because the FM session is rebuilt
    /// per send, this single hook covers boot + every turn + post-
    /// compaction for free — the model's durable memory is ALWAYS
    /// present, never recalled (the "always inject" principle).
    func fullInstructions(agent: Agent, kernel: Kernel) async -> String {
        await Self.fullInstructions(agent: agent, kernel: kernel)
    }

    fileprivate static func fullInstructions(agent: Agent, kernel: Kernel) async -> String {
        var blocks: [String] = []
        let base = agent.metaValue(forKey: "instructions")?.asString ?? ""
        if !base.isEmpty { blocks.append(base) }
        let memAgentIds =
            agent.childIds().filter {
                kernel.agent($0)?.handlerModule == "durable_memory.tools"
            }
        for memAgentId in memAgentIds {
            // durable_memory is modeless and has no whole-store verb (full parity
            // with python), so reconstruct the block from `keys` + `read`.
            let keysReply = await kernel.send([Item(memAgentId, .object(["type": .string("keys")]))])[0].result
            let keys = (keysReply["keys"].asArray ?? []).compactMap { $0.asString }
            var lines: [String] = []
            for key in keys {
                let r = await kernel.send([Item(memAgentId, .object(["type": .string("read"), "key": .string(key)]))])[0].result
                if r["value"].isNull { continue }
                lines.append("\(key): \(r["value"].serialize())")
            }
            guard !lines.isEmpty else { continue }
            let label = kernel.agent(memAgentId)?.metaValue(forKey: "mode")?.asString ?? "durable"
            blocks.append("## Your \(label) memory (\(memAgentId.value)):\n" + lines.joined(separator: "\n"))
        }
        return blocks.joined(separator: "\n\n")
    }

    /// Generation temperature in [0, 2]. Read from the agent's
    /// `meta.temperature` field; defaults to 0.4 (WWDC25 guidance for
    /// tool-grounded factual answers).
    fileprivate static func temperature(agent: Agent) -> Double {
        if let dbl = agent.metaValue(forKey: "temperature")?.asDouble {
            return dbl
        }
        if let i = agent.metaValue(forKey: "temperature")?.asInt {
            return Double(i)
        }
        return 0.4
    }

    // MARK: - Availability + model identity (FM gating isolated here)

    static func isAvailable() -> Bool {
        #if canImport(FoundationModels)
            if #available(macOS 26, iOS 26, visionOS 26, *) {
                return SystemLanguageModel.default.availability == .available
            }
        #endif
        return false
    }

    static func unavailableReason() -> String {
        #if canImport(FoundationModels)
            if #available(macOS 26, iOS 26, visionOS 26, *) {
                switch SystemLanguageModel.default.availability {
                case .available:
                    return "ok"
                case .unavailable(let reason):
                    switch reason {
                    case .appleIntelligenceNotEnabled:
                        return "apple_intelligence_not_enabled"
                    case .deviceNotEligible:
                        return "device_not_eligible"
                    case .modelNotReady:
                        return "model_not_ready"
                    @unknown default:
                        return "unavailable_unknown"
                    }
                @unknown default:
                    return "unavailable_unknown"
                }
            }
            return "os_version_too_old"
        #else
            return "framework_not_available"
        #endif
    }

    static func modelLabel() -> String {
        #if canImport(FoundationModels)
            return "apple_system_language_model"
        #else
            return "n/a"
        #endif
    }
}

/// Small box so the `backendStateExtra` closure can reach the shared
/// core for its live in-flight count without a capture cycle.
private final class WeakBox: @unchecked Sendable {
    weak var value: AIBackend?
}
