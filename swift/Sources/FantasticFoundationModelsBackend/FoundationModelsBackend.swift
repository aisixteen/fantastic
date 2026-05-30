// Apple Foundation Models LLM backend — kernel-native bundle.
//
// Wraps Apple's `FoundationModels` framework (`LanguageModelSession`)
// behind the same verb surface as `OllamaBackend` + `NvidiaNimBackend`,
// so chat UIs can speak to Apple FM exactly the way they speak to
// any other LLM backend in the kernel.
//
// Design: ATOMIC + STATELESS. Every `send` builds a fresh
// `LanguageModelSession(instructions:tools:)`, feeds it ONE user
// message, streams the reply, lets ARC drop the session. No
// transcript carries between calls. The 3B on-device model has a
// 4096-token window — treating each call as `(system + tools +
// input) → reply` keeps the full budget available every time and
// architecturally avoids the long-context drift small models hit
// inside multi-turn chats.
//
// Concurrency: Apple's `generativeexperiencesd` daemon serializes
// inference at the hardware layer across the whole OS — see Apple's
// developer forum thread #798113 ("You can follow the Swift
// concurrency rules to run multiple Foundation Models sessions /
// tasks concurrently. The framework doesn't impose any extra rules
// for concurrency."). No in-process gate is needed; concurrent
// `fm` agents in one process Just Work.
//
// Platform gating: compile guarded by `canImport(FoundationModels)`
// + runtime `if #available`. On platforms / OS versions where the
// framework isn't available (anything below iOS 26 / macOS 26 /
// visionOS 26, or any tvOS / watchOS), the bundle compiles to a
// stub that always reports `available: false` and rejects `send`.
//
// Agent meta-field contract:
//
//   meta.instructions  String  — system prompt body. Consumer owns
//                                the content; the bundle does no
//                                templating. Defaults to `""`.
//   meta.temperature   Number  — generation temperature in [0, 2].
//                                Defaults to 0.4 (WWDC25 guidance
//                                for tool-grounded factual answers;
//                                Apple's default ~0.8 is "inconsistent
//                                and nonsensical" in their language).
//
// Tool catalog wired into every session (constant across calls):
//
//   reflect(agentId)          inspect one kernel agent
//   list_agents()             enumerate every agent (one-call roster)
//   list_proxy_hosts()        proxy_agent surfaces + host_registered
//   create_agent(handler)     create + return new agent id
//   delete_agent(agentId)     cascade-delete an agent
//
// History (in-memory per `(agent_id, client_id)`) is kept ONLY so
// the `history` verb can return the UI bubble log. It is NEVER fed
// back to the model. Cross-call memory is a future `fm_memory.tools`
// concern (read via tool call, not via session transcript).

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
    public init() {}

    /// Per-(agent_id, client_id) chat history. Matches the keying
    /// scheme `OllamaBackend` uses so chat UIs see identical
    /// `history` replies across backends.
    private let historyLock = NSLock()
    private var history: [String: [JSON]] = [:]

    /// Cancellation state — epoch-bump pattern. `interrupt` bumps
    /// `interruptEpoch`; every stream records the epoch it started
    /// under, polls `isStreamCancelled` each tick. Stream is
    /// cancelled iff its `startedAt` epoch is older than the
    /// current epoch OR its stream id is explicitly in
    /// `cancelledStreams`.
    ///
    /// Why epochs and not a `*` sentinel-in-set: with a sentinel,
    /// the first cancelled stream's `defer` would clear the sentinel
    /// out from under other in-flight streams that haven't polled
    /// yet. Found by app-side `interruptClearsQueue` test —
    /// epoch counters are race-free against staggered stream-death
    /// AND don't bleed into post-interrupt sends (a new send records
    /// the new epoch and is unaffected by prior interrupts).
    private let cancelLock = NSLock()
    private var interruptEpoch: UInt64 = 0
    private var cancelledStreams: Set<String> = []

    /// In-flight stream count for `backend_state` reporting. The
    /// bundle doesn't queue (atomic stateless model — each send is
    /// its own Task), so there's no `queue_depth` to report.
    private let stateLock = NSLock()
    private var inFlight: Int = 0

    // NOTE: No cached `LanguageModelSession`. The bundle operates in
    // STATELESS mode — every `send` builds a fresh `LanguageModelSession`,
    // uses it once, lets ARC drop it. The 3B model has a 4096-token
    // window; chat-style transcript reuse fills that budget within
    // ~15 dense turns. Treating each request as an independent
    // function call sidesteps the wall entirely AND avoids the
    // `AppGraph.shared` second-session-tool-call crash (only one
    // session ever exists at a time, naturally serialized by the
    // user typing one prompt at a time). Memory across calls — if
    // ever needed — lives in the `fm_memory.tools` bundle and is
    // pulled via tool call into the single-turn context, NOT via
    // session transcript.

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent")])
        }
        switch verb {
        case "reflect":
            return [
                "id": .string(agent.id.value),
                "sentence": .string("Apple Foundation Models LLM agent (on-device, native tool-calling)."),
                "kind": .string("foundation_models_backend"),
                "provider": .string("apple_foundation_models"),
                "available": .bool(isAvailable()),
                "model": .string(modelLabel()),
                "verbs": [
                    "send": "args: text, client_id?. Atomic stateless call: (system + tools + text) → streaming reply.",
                    "history": "args: client_id?. Returns UI bubble log (not fed back to the model).",
                    "interrupt": "args: client_id?. Cancels in-flight stream.",
                    "backend_state": "Reports availability + in-flight count.",
                ] as JSON,
            ] as JSON
        case "boot":
            await mountMemoryAgents(agentId: agentId, kernel: kernel)
            return .object(["ok": .bool(true)])
        case "shutdown":
            return .object(["ok": .bool(true)])
        case "send":
            return await sendVerb(agent: agent, payload: payload, kernel: kernel)
        case "history":
            return historyVerb(agent: agent, payload: payload)
        case "interrupt":
            return interruptVerb(payload: payload)
        case "backend_state":
            return backendStateReply()
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    // MARK: - Verbs

    private func sendVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        guard let text = payload["text"].asString else {
            return .object(["error": .string("send requires text")])
        }
        let clientId = payload["client_id"].asString ?? "cli"

        guard isAvailable() else {
            return .object([
                "error": .string("foundation_models_unavailable"),
                "reason": .string(unavailableReason()),
            ])
        }

        let streamId = "stm_\(UUID().uuidString.prefix(8))"
        let messageId = "msg_\(UUID().uuidString.prefix(8))"
        let userMessageId = "msg_\(UUID().uuidString.prefix(8))"

        appendHistory(
            key: historyKey(agent: agent.id, client: clientId),
            message: .object([
                "id": .string(userMessageId),
                "role": .string("user"),
                "content": .string(text),
                "complete": .bool(true),
            ]))

        // Spawn the per-request Task. With atomic stateless calls,
        // each Task builds its own `LanguageModelSession`, runs one
        // generation, lets ARC drop the session on return.
        bumpInFlight(+1)
        Task { [weak self, weak kernel] in
            guard let self = self, let kernel = kernel else { return }
            await self.runStream(
                streamId: streamId,
                messageId: messageId,
                agentId: agent.id,
                clientId: clientId,
                userText: text,
                kernel: kernel
            )
        }

        return .object([
            "queued": .bool(true),
            "stream_id": .string(streamId),
            "message_id": .string(messageId),
            "client_id": .string(clientId),
        ])
    }

    private func runStream(
        streamId: String,
        messageId: String,
        agentId: AgentId,
        clientId: String,
        userText: String,
        kernel: Kernel
    ) async {
        defer {
            clearStreamCancel(streamId)
            bumpInFlight(-1)
        }

        #if canImport(FoundationModels)
            if #available(macOS 26, iOS 26, visionOS 26, *) {
                await runLiveStream(
                    streamId: streamId,
                    messageId: messageId,
                    agentId: agentId,
                    clientId: clientId,
                    userText: userText,
                    kernel: kernel
                )
                return
            }
        #endif

        // Framework unavailable at runtime — emit an error done event.
        await kernel.emit(
            agentId,
            .object([
                "type": .string("done"),
                "stream_id": .string(streamId),
                "message_id": .string(messageId),
                "error": .string("foundation_models_unavailable"),
                "client_id": .string(clientId),
            ]))
        _ = userText  // silence unused; real run consumes it
    }

    private func historyVerb(agent: Agent, payload: JSON) -> JSON {
        let clientId = payload["client_id"].asString ?? "cli"
        let messages = readHistory(key: historyKey(agent: agent.id, client: clientId))
        return .object([
            "messages": .array(messages),
            "client_id": .string(clientId),
        ])
    }

    private func interruptVerb(payload: JSON) -> JSON {
        // Bump the interrupt epoch. Every currently-in-flight stream
        // recorded a `startedAt` epoch when it began; bumping makes
        // every prior epoch strictly less than the current one, so
        // `isStreamCancelled` returns true for all of them. New
        // sends arriving AFTER this point record the new epoch and
        // are unaffected.
        cancelLock.lock()
        interruptEpoch &+= 1
        cancelLock.unlock()
        _ = payload  // client_id reserved for per-stream cancel later
        return .object(["interrupted": .bool(true)])
    }

    private func backendStateReply() -> JSON {
        return .object([
            "provider": .string("apple_foundation_models"),
            "apple_intelligence_available": .bool(isAvailable()),
            "model_available": .bool(isAvailable()),
            "backend_registered": .bool(true),
            "model": .string(modelLabel()),
            "in_flight": .integer(Int64(readInFlight())),
            "reason": .string(isAvailable() ? "ok" : unavailableReason()),
        ])
    }

    // MARK: - Availability + model identity

    /// True when the FoundationModels framework is reachable AND
    /// the system model reports `.available` at runtime. Compile-
    /// gated by `canImport`; runtime-gated by `#available` + the
    /// `SystemLanguageModel.default.availability` enum.
    private func isAvailable() -> Bool {
        #if canImport(FoundationModels)
            if #available(macOS 26, iOS 26, visionOS 26, *) {
                return SystemLanguageModel.default.availability == .available
            }
        #endif
        return false
    }

    /// Human-readable reason for unavailability. Mirrors the
    /// reasons the app's chat UI surfaces today.
    private func unavailableReason() -> String {
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

    private func modelLabel() -> String {
        #if canImport(FoundationModels)
            // Apple FM ships one on-device system model today —
            // no per-instance model id is exposed via the public
            // API. Label is stable for telemetry.
            return "apple_system_language_model"
        #else
            return "n/a"
        #endif
    }

    /// System-prompt body for the LLM session. Read from the
    /// agent's `meta.instructions` field at session-build time
    /// (currently unused — the live FM port reads + applies this
    /// in the follow-up). Empty string is treated the same as
    /// "no instructions" by Apple FM.
    private func instructions(agent: Agent) -> String {
        agent.metaValue(forKey: "instructions")?.asString ?? ""
    }

    /// Idempotently mount `mem` + `data` yaml_state memory agents under
    /// this backend at boot, so the model's durable memory exists — and
    /// is auto-injected — before turn one. No-op if an agent of each mode
    /// already exists (children rehydrate from disk on reboot).
    func mountMemoryAgents(agentId: AgentId, kernel: Kernel) async {
        let have = Set(
            memoryAgents(agentId: agentId, kernel: kernel).compactMap {
                kernel.agent($0)?.metaValue(forKey: "mode")?.asString
            })
        for mode in ["mem", "data"] where !have.contains(mode) {
            _ = await kernel.send(
                agentId,
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string("yaml_state.tools"),
                    "mode": .string(mode),
                ]))
        }
    }

    /// Agent ids of this backend's mounted yaml_state memory agents.
    func memoryAgents(agentId: AgentId, kernel: Kernel) -> [AgentId] {
        guard let agent = kernel.agent(agentId) else { return [] }
        return agent.childIds().filter {
            kernel.agent($0)?.handlerModule == "yaml_state.tools"
        }
    }

    /// The system-prompt body + each mounted yaml_state memory agent's current
    /// `state_yaml` spliced in. Because the FM session is rebuilt per
    /// send, this single hook covers boot + every turn + post-compaction
    /// for free — the model's durable memory is ALWAYS present, never
    /// recalled (the "always inject" principle). Substrate-enforced
    /// recall: the model supplies content via `set`; reading is structural.
    func fullInstructions(agent: Agent, kernel: Kernel) async -> String {
        var blocks: [String] = []
        let base = instructions(agent: agent)
        if !base.isEmpty { blocks.append(base) }
        for memAgentId in memoryAgents(agentId: agent.id, kernel: kernel) {
            let reply = await kernel.send(memAgentId, .object(["type": .string("state_yaml")]))
            guard let yaml = reply["yaml"].asString, !yaml.isEmpty else { continue }
            let mode = kernel.agent(memAgentId)?.metaValue(forKey: "mode")?.asString ?? "data"
            blocks.append("## Your \(mode) memory (\(memAgentId.value)):\n\(yaml)")
        }
        return blocks.joined(separator: "\n\n")
    }

    /// Generation temperature in [0, 2]. Read from the agent's
    /// `meta.temperature` field; defaults to 0.4 (WWDC25 guidance
    /// for tool-grounded factual answers). Anything outside the
    /// valid range is clamped at apply-time in the follow-up.
    private func temperature(agent: Agent) -> Double {
        if let dbl = agent.metaValue(forKey: "temperature")?.asDouble {
            return dbl
        }
        if let i = agent.metaValue(forKey: "temperature")?.asInt {
            return Double(i)
        }
        return 0.4
    }

    // MARK: - History / cancel / state helpers (NSLock-protected)

    private func historyKey(agent: AgentId, client: String) -> String {
        "\(agent.value)|\(client)"
    }

    private func appendHistory(key: String, message: JSON) {
        historyLock.lock()
        defer { historyLock.unlock() }
        var arr = history[key] ?? []
        arr.append(message)
        history[key] = arr
    }

    private func readHistory(key: String) -> [JSON] {
        historyLock.lock()
        defer { historyLock.unlock() }
        return history[key] ?? []
    }

    /// Snapshot the current epoch — call at the very top of a
    /// runLiveStream so the stream's birth epoch is fixed before
    /// the first `isStreamCancelled` poll.
    fileprivate func currentEpoch() -> UInt64 {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return interruptEpoch
    }

    /// Returns true iff the stream should bail. A stream is
    /// cancelled if (a) its id was explicitly added to
    /// `cancelledStreams` (reserved for future per-stream cancel),
    /// or (b) an interrupt fired AFTER the stream started
    /// (epoch advanced past `startedAt`).
    fileprivate func isStreamCancelled(streamId: String, startedAt: UInt64)
        -> Bool
    {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        return cancelledStreams.contains(streamId) || startedAt < interruptEpoch
    }

    /// Drop the per-stream id from `cancelledStreams` (no-op if it
    /// was never added). Does NOT touch the epoch — that stays
    /// monotonic across the bundle's lifetime, which is the whole
    /// point of the epoch-bump pattern.
    fileprivate func clearStreamCancel(_ id: String) {
        cancelLock.lock()
        defer { cancelLock.unlock() }
        cancelledStreams.remove(id)
    }

    private func bumpInFlight(_ delta: Int) {
        stateLock.lock()
        defer { stateLock.unlock() }
        inFlight = max(0, inFlight + delta)
    }

    private func readInFlight() -> Int {
        stateLock.lock()
        defer { stateLock.unlock() }
        return inFlight
    }
}

#if canImport(FoundationModels)

    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    extension FoundationModelsBackendBundle {

        /// Live Apple Foundation Models generation path. Minimal —
        /// single cached session, no queue, no retry, no compaction,
        /// no transcript persistence. Just enough to probe viability:
        /// build a session with the kernel's reflect tool, stream
        /// tokens, emit them on the agent's inbox. Production-grade
        /// retry + session-router + transcript sidecar land in the
        /// follow-up (see project_fm_bundle_memory_blocker memory).
        fileprivate func runLiveStream(
            streamId: String,
            messageId: String,
            agentId: AgentId,
            clientId: String,
            userText: String,
            kernel: Kernel
        ) async {
            // Pull instructions + temperature from the agent's meta.
            guard let agent = kernel.agent(agentId) else {
                await emitError(
                    kernel: kernel, agentId: agentId,
                    streamId: streamId, messageId: messageId,
                    clientId: clientId, error: "agent_disappeared"
                )
                return
            }
            let inst = await fullInstructions(agent: agent, kernel: kernel)
            let temp = temperature(agent: agent)

            // Snapshot the epoch BEFORE building the session — fixes
            // the stream's "birth time" so subsequent interrupts are
            // detected via `startedAt < interruptEpoch`.
            let startedAtEpoch = currentEpoch()

            // Fresh session every send — see the stateless note above.
            let session = freshSession(instructions: inst, kernel: kernel)
            let options = GenerationOptions(temperature: temp)

            var accumulated = ""
            var pushedLength = 0

            do {
                let stream = session.streamResponse(to: userText, options: options)
                for try await partial in stream {
                    // Honor `interrupt` between snapshots. The stream
                    // iterator itself doesn't observe Swift task
                    // cancellation under Apple FM, so the loop has
                    // to poll explicitly. Epoch-bump pattern: any
                    // interrupt that fired AFTER this stream started
                    // shows up as `startedAt < interruptEpoch`.
                    if isStreamCancelled(streamId: streamId, startedAt: startedAtEpoch) {
                        await emitError(
                            kernel: kernel, agentId: agentId,
                            streamId: streamId, messageId: messageId,
                            clientId: clientId, error: "interrupted"
                        )
                        return
                    }
                    // Apple FM yields cumulative snapshots — extract
                    // the new suffix since the last push.
                    let content = String(describing: partial.content)
                    if content.count > pushedLength {
                        let startIdx = content.index(
                            content.startIndex, offsetBy: pushedLength)
                        let delta = String(content[startIdx...])
                        pushedLength = content.count
                        accumulated = content
                        await kernel.emit(
                            agentId,
                            .object([
                                "type": .string("token"),
                                "stream_id": .string(streamId),
                                "message_id": .string(messageId),
                                "delta": .string(delta),
                                "accumulated": .string(accumulated),
                                "client_id": .string(clientId),
                            ]))
                    }
                }
            } catch {
                // Stateless mode — nothing to clean up. The session
                // is local to this function and ARC drops it when
                // we return. Next send builds a fresh one anyway.
                await emitError(
                    kernel: kernel, agentId: agentId,
                    streamId: streamId, messageId: messageId,
                    clientId: clientId, error: "\(error)"
                )
                return
            }

            await kernel.emit(
                agentId,
                .object([
                    "type": .string("done"),
                    "stream_id": .string(streamId),
                    "message_id": .string(messageId),
                    "accumulated": .string(accumulated),
                    "client_id": .string(clientId),
                ]))

            appendHistory(
                key: historyKey(agent: agentId, client: clientId),
                message: .object([
                    "id": .string(messageId),
                    "role": .string("assistant"),
                    "content": .string(accumulated),
                    "complete": .bool(true),
                ]))
        }

        /// Build a fresh `LanguageModelSession` per send. Stateless
        /// mode — never cached. See the note on the bundle's
        /// `sessionStorage` deletion for rationale.
        ///
        /// Tool catalog today: one introspection primitive
        /// (`reflect`) plus two aggregators (`list_agents`,
        /// `list_proxy_hosts`) that solve the per-turn tool-call-cap
        /// problem by collapsing N reflects into 1 call.
        fileprivate func freshSession(
            instructions inst: String,
            kernel: Kernel
        ) -> LanguageModelSession {
            let tools: [any Tool] = [
                KernelReflectTool(kernel: kernel),
                KernelListAgentsTool(kernel: kernel),
                KernelListProxyHostsTool(kernel: kernel),
                KernelCreateAgentTool(kernel: kernel),
                KernelDeleteAgentTool(kernel: kernel),
            ]
            if inst.isEmpty {
                return LanguageModelSession(tools: tools)
            }
            return LanguageModelSession(
                tools: tools,
                instructions: { Instructions(inst) }
            )
        }

        fileprivate func emitError(
            kernel: Kernel, agentId: AgentId,
            streamId: String, messageId: String,
            clientId: String, error: String
        ) async {
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("done"),
                    "stream_id": .string(streamId),
                    "message_id": .string(messageId),
                    "error": .string(error),
                    "client_id": .string(clientId),
                ]))
        }
    }

    /// Single canvas reflect tool wired into every Apple FM session.
    /// The model calls this to inspect ANY kernel agent by id —
    /// route through `kernel.send(<agentId>, {type:"reflect"})`,
    /// return the JSON reply as a string for the model to read.
    ///
    /// Initial-probe scope: this is the ONLY tool we register. Later
    /// commits replace this with a dynamic-tool wrapper that pulls
    /// the full kernel `tools.tools` registry at session-build (the
    /// `KernelTool` pattern app-claude described in their brief).
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelReflectTool: Tool {
        let kernel: Kernel

        var name: String { "reflect" }
        var description: String {
            """
            Inspect the state of a kernel agent. Returns the agent's
            identity, kind, and any verb-specific fields. Use this
            before answering questions about the kernel — never
            invent agent IDs.
            """
        }

        @Generable
        struct Arguments {
            @Guide(
                description:
                    "The id of the agent to inspect, e.g. 'core', 'chat_ui', 'fm'.")
            let agentId: String
        }

        func call(arguments: Arguments) async throws -> String {
            let id = arguments.agentId.trimmingCharacters(in: .whitespaces)
            if id.isEmpty {
                return #"{"error":"empty agent id"}"#
            }
            let reply = await kernel.send(
                AgentId(id),
                .object(["type": .string("reflect")])
            )
            return reply.serialize()
        }
    }

    /// Aggregator: enumerate every agent in one call AND reflect-
    /// fan-out under the hood so each entry carries `kind` and
    /// `sentence` from its own reflect reply. Gives the model a
    /// grounded description per agent without forcing it to chain
    /// reflects (which hits Apple FM's per-turn tool-call ceiling).
    ///
    /// Returns `{agents:[{id, handler_module, kind, sentence}]}`.
    /// `sentence` may be `null` for agents whose bundle's reflect
    /// doesn't emit one — model should answer "no description
    /// available" in that case per system prompt rule 3.
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelListAgentsTool: Tool {
        let kernel: Kernel

        var name: String { "list_agents" }
        var description: String {
            """
            Enumerate every agent in the kernel with its kind and \
            short description in a single tool call. Returns \
            {agents:[{id, handler_module, kind, sentence}]} where \
            `sentence` is the agent's self-description (or null if \
            the bundle doesn't expose one). Quote sentences \
            verbatim — do NOT invent descriptions for entries \
            whose sentence is null. Use this BEFORE answering any \
            "what agents exist / what does X do" question.
            """
        }

        @Generable
        struct Arguments {}

        func call(arguments: Arguments) async throws -> String {
            let listReply = await kernel.send(
                AgentId("core"),
                .object(["type": .string("list_agents")])
            )
            guard let agents = listReply["agents"].asArray else {
                return listReply.serialize()
            }
            var enriched: [JSON] = []
            for entry in agents {
                guard let id = entry["id"].asString else { continue }
                let reflectReply = await kernel.send(
                    AgentId(id),
                    .object(["type": .string("reflect")])
                )
                var record: OrderedDictionary<String, JSON> = [:]
                record["id"] = .string(id)
                record["handler_module"] = entry["handler_module"]
                record["kind"] = reflectReply["kind"]
                record["sentence"] = reflectReply["sentence"]
                enriched.append(.object(record))
            }
            return JSON.object(["agents": .array(enriched)]).serialize()
        }
    }

    /// Aggregator: enumerate proxy_agent surfaces and report which
    /// have a host registered. Solves the "which UI agents are
    /// active" question in one call instead of N reflects.
    ///
    /// Implementation: query `core.list_agents`, filter to entries
    /// whose handler_module is `proxy_agent.tools`, reflect each in
    /// parallel, return a flat list. This DOES still call reflect
    /// N times under the hood, but FM-side it's one tool call → one
    /// result. The model never sees the underlying fanout.
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelListProxyHostsTool: Tool {
        let kernel: Kernel

        var name: String { "list_proxy_hosts" }
        var description: String {
            """
            Enumerate every proxy_agent surface in the kernel and \
            report each one's host_registered status. Returns \
            [{id, host_registered}]. Use this whenever the user \
            asks about UI surface availability — it avoids the \
            per-turn tool-call ceiling that would truncate a chain \
            of reflect calls.
            """
        }

        @Generable
        struct Arguments {}

        func call(arguments: Arguments) async throws -> String {
            let listReply = await kernel.send(
                AgentId("core"),
                .object(["type": .string("list_agents")])
            )
            guard let agents = listReply["agents"].asArray else {
                return #"{"error":"list_agents returned non-array"}"#
            }
            var results: [JSON] = []
            for a in agents {
                guard a["handler_module"].asString == "proxy_agent.tools",
                    let id = a["id"].asString
                else { continue }
                let reflectReply = await kernel.send(
                    AgentId(id),
                    .object(["type": .string("reflect")])
                )
                results.append(
                    .object([
                        "id": .string(id),
                        "host_registered": reflectReply["host_registered"],
                    ]))
            }
            return JSON.object(["proxy_hosts": .array(results)]).serialize()
        }
    }

    /// Write tool: create a new agent. Forwards to
    /// `kernel.send("core", {type:"create_agent", handler_module:…})`.
    /// Returns the new agent's id (or an error).
    ///
    /// Auto-generated id only — the model rarely needs to set a
    /// specific id, and stringly-typed user-supplied ids are a
    /// hallucination risk ("create file_abc" → kernel rejects).
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelCreateAgentTool: Tool {
        let kernel: Kernel

        var name: String { "create_agent" }
        var description: String {
            """
            Create a new agent in the kernel. Returns the new \
            agent's auto-generated id on success, or {error:…} if \
            the handler_module is unknown. Common handler modules: \
            file.tools, html_agent.tools, scheduler.tools, \
            canvas_backend.tools, canvas_webapp.tools, \
            ai_chat_webapp.tools, terminal_webapp.tools.
            """
        }

        @Generable
        struct Arguments {
            @Guide(
                description:
                    "The handler_module string, e.g. 'file.tools' or 'canvas_backend.tools'.")
            let handlerModule: String
        }

        func call(arguments: Arguments) async throws -> String {
            let hm = arguments.handlerModule.trimmingCharacters(in: .whitespaces)
            if hm.isEmpty {
                return #"{"error":"empty handler_module"}"#
            }
            let reply = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string(hm),
                ]))
            return reply.serialize()
        }
    }

    /// Write tool: delete an agent (and cascade-delete its
    /// children). Forwards to `kernel.send("core", {type:
    /// "delete_agent", id:…})`.
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelDeleteAgentTool: Tool {
        let kernel: Kernel

        var name: String { "delete_agent" }
        var description: String {
            """
            Delete an agent from the kernel by id. Cascade-deletes \
            its children. Returns {ok:true} on success or \
            {error:…, locked:true} if the agent has delete_lock \
            set. Cannot be undone — use sparingly.
            """
        }

        @Generable
        struct Arguments {
            @Guide(description: "The id of the agent to delete.")
            let agentId: String
        }

        func call(arguments: Arguments) async throws -> String {
            let id = arguments.agentId.trimmingCharacters(in: .whitespaces)
            if id.isEmpty {
                return #"{"error":"empty agent id"}"#
            }
            let reply = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("delete_agent"),
                    "id": .string(id),
                ]))
            return reply.serialize()
        }
    }

#endif
