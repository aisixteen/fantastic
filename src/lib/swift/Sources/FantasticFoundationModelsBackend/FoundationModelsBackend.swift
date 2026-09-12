// Apple Foundation Models LLM backend — kernel-native bundle.
//
// Wraps Apple's `FoundationModels` framework (`LanguageModelSession`)
// behind the same verb surface as the ollama + NIM backends, so chat
// UIs speak to Apple FM exactly the way they speak to any other LLM
// backend in the kernel.
//
// Every `send` builds a fresh `LanguageModelSession(instructions:tools:)`
// and streams the reply; the CONVERSATION is the shared core's, exactly as
// for every other backend: the row store is the history, the view/fold/
// recall path projects it into the model's window, and the session is
// handed the projected messages. The 3B on-device model has a 4096-token
// window, so the projection matters more here than anywhere.
//
// The model's DURABLE memory is the always-inject path: at `boot` the
// bundle self-mounts a private memory FILE surface (a `file_bridge`
// child) plus a `text_file` editor over it, and the session
// instructions splice each memory document's content in on EVERY
// send. Recall is structural, not prompted; the format is the
// model's own (memory is plain files).
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
                compactPrompt: true,
                reflectExtra: { _ in
                    [
                        "available": .bool(Self.isAvailable()),
                        "model": .string(Self.modelLabel()),
                    ]
                },
                // Always-inject durable memory: the shared core appends
                // this AFTER the substrate prompt. Each memory DOCUMENT's
                // content + any custom `instructions` meta land in the FM
                // session's instructions every turn — recall is structural.
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
    }

    public var readme: String? {
        """
        foundation_models_backend — Apple on-device Foundation Models LLM backend; thin over FantasticAICore.
        Verbs: the shared ai_core set (reflect/send/inquiry/history/status/context_status/recall/pull/grep/context_recall). \
        Same LLM backend contract as ollama_backend, against the native on-device model.
        Tool-calling is RAW: no Apple @Generable Tool — the model generates text and \
        FantasticAICore parses the <call name=…> envelope (tool_parse).
        Durable memory: on boot a private memory file surface + a text_file editor are \
        auto-mounted; every memory document's content is spliced into the model \
        instructions on every send (always-on; no explicit recall needed).
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

    /// Idempotently mount the memory FILE surface + its `text_file` editor
    /// under this backend at boot, so the model's durable memory exists — and
    /// is auto-injected — before turn one. Idempotent by id (children
    /// rehydrate from disk on reboot).
    func mountMemoryAgents(agentId: AgentId, kernel: Kernel) async {
        // Self-wire THIS backend's own `file_bridge_id` to the discovered store
        // so the shared core persists the chat JSONL sidecar through it (FM is
        // self-bootstrapping; ollama/NIM expect the operator to set this). Only
        // when absent + a store exists; no store ⇒ history stays RAM-empty.
        let storeId = kernel.findStore()?.value
        if let storeId, kernel.agent(agentId)?.metaValue(forKey: "file_bridge_id") == nil {
            _ = await kernel.send([Item(agentId, .object([
                    "type": .string("update_agent"),
                    "id": .string(agentId.value),
                    "file_bridge_id": .string(storeId),
                ]))])[0].result
        }

        // MEMORY IS FILES: mount a private file surface (clamped inside the
        // running dir) + a text_file editor over it. Idempotent by id —
        // children rehydrate from disk on reboot.
        let memFbId = "\(agentId.value)_memory_files"
        let edId = "\(agentId.value)_memory"
        if kernel.agent(AgentId(memFbId)) == nil {
            let rec: JSON = .object([
                "type": .string("create_agent"),
                "parent_id": .string(agentId.value),
                "handler_module": .string("file_bridge.tools"),
                "id": .string(memFbId),
                "root": .string(".fantastic/agents/\(agentId.value)/memory"),
                // The io seal gates every verb until opened; the editor is the
                // intended caller and the two-sided call gate still applies.
                "ingress_rule": .string("allow_all_ingress"),
                "callable_by": .object([edId: .string("*")]),
                "description": .string(
                    "The FM backend's private memory surface — raw byte streams "
                        + "under its own directory."),
            ])
            // Boot-time self-provisioning runs FROM ABOVE (operator).
            _ = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([Item(AgentId("kernel"), rec)])[0].result
            }
        }
        if kernel.agent(AgentId(edId)) == nil {
            let rec: JSON = .object([
                "type": .string("create_agent"),
                "parent_id": .string(agentId.value),
                "handler_module": .string("text_file.tools"),
                "id": .string(edId),
                "target_id": .string(memFbId),
                "can_call": .object([memFbId: .string("*")]),
                "description": .string(
                    "The model's durable memory — plain text documents, any "
                        + "format (write/patch/append/read); every document's "
                        + "content is injected into the instructions each turn."),
            ])
            _ = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([Item(AgentId("kernel"), rec)])[0].result
            }
        }
    }

    /// Agent ids of this backend's mounted memory EDITORS (`text_file`
    /// children). The chat transcript is a JSONL sidecar file, not an agent,
    /// so nothing needs excluding any more.
    func memoryAgents(agentId: AgentId, kernel: Kernel) -> [AgentId] {
        guard let agent = kernel.agent(agentId) else { return [] }
        return agent.childIds().filter {
            kernel.agent($0)?.handlerModule == "text_file.tools"
        }
    }

    /// The system-prompt body + each memory DOCUMENT's current content
    /// spliced in. Because the FM session is rebuilt
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
        // Memory is FILES: for each mounted text_file editor, list its surface
        // and splice every (small) document's content in verbatim. The format
        // inside each document is the model's own — nothing is parsed.
        let editors = agent.childIds().filter {
            kernel.agent($0)?.handlerModule == "text_file.tools"
        }
        for edId in editors {
            guard
                let surface = kernel.agent(edId)?.metaValue(forKey: "target_id")?.asString,
                !surface.isEmpty
            else { continue }
            let listing = await KernelTaskLocals.$currentSender.withValue(nil) {
                await kernel.send([
                    Item(AgentId(surface), .object(["type": .string("list"), "path": .string("")]))
                ])[0].result
            }
            var docs: [String] = []
            for f in listing["files"].asArray ?? [] {
                guard f["type"].asString == "file", let name = f["name"].asString,
                    (f["size"].asInt ?? 0) <= 16384
                else { continue }
                let (r, body) = await KernelTaskLocals.$currentSender.withValue(nil) {
                    await kernel.sendWithBinary(
                        AgentId(surface),
                        .object([
                            "type": .string("read_stream"), "path": .string(name),
                        ]), Data())
                }
                guard r["error"].asString == nil,
                    let text = String(data: body, encoding: .utf8), !text.isEmpty
                else { continue }
                docs.append("### \(name)\n\(text)")
            }
            guard !docs.isEmpty else { continue }
            blocks.append(
                "## Your memory (edit via `\(edId.value)`):\n" + docs.joined(separator: "\n\n"))
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
