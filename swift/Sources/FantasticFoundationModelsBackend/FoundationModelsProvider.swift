// FoundationModelsProvider — the Apple-FM `AIProvider` impl.
//
// This is the ONLY adapter file beyond FoundationModelsBackend.swift
// that touches the FoundationModels framework. All `#if canImport`
// / `#available` gating is contained here; the shared
// `FantasticAICore` core never imports FoundationModels.
//
// Stateless: each `chat()` builds a fresh `LanguageModelSession`,
// streams cumulative on-device snapshots, extracts the new suffix per
// snapshot, yields it as an `AIChunk.token`. Interrupt is honoured by
// the shared core's per-chunk cancel poll — the provider simply
// finishes its stream when the consuming task is cancelled.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

#if canImport(FoundationModels)
    import FoundationModels
#endif

struct FoundationModelsProvider: AIProvider {
    let instructions: String
    let temperature: Double
    let kernel: Kernel

    var model: String { "apple_system_language_model" }

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        // The shared core supplies the assembled messages; in stateless
        // mode that's just the single user turn. Extract its content.
        let userText = messages.last?["content"].asString ?? ""
        let instructions = instructions
        let temperature = temperature
        let kernel = kernel

        return AsyncThrowingStream { continuation in
            #if canImport(FoundationModels)
                if #available(macOS 26, iOS 26, visionOS 26, *) {
                    let task = Task {
                        let session = Self.freshSession(
                            instructions: instructions, kernel: kernel)
                        let options = GenerationOptions(temperature: temperature)
                        var pushedLength = 0
                        do {
                            let stream = session.streamResponse(to: userText, options: options)
                            for try await partial in stream {
                                if Task.isCancelled { break }
                                // Apple FM yields cumulative snapshots —
                                // extract the new suffix since last push.
                                let content = String(describing: partial.content)
                                if content.count > pushedLength {
                                    let startIdx = content.index(
                                        content.startIndex, offsetBy: pushedLength)
                                    let delta = String(content[startIdx...])
                                    pushedLength = content.count
                                    continuation.yield(.token(delta))
                                }
                            }
                            continuation.finish()
                        } catch {
                            continuation.finish(throwing: error)
                        }
                    }
                    continuation.onTermination = { _ in task.cancel() }
                    return
                }
            #endif
            // Framework unavailable at runtime. `makeProvider` already
            // refused on the unavailable path, so this is defensive only.
            continuation.finish(throwing: FMError.unavailable)
        }
    }

    #if canImport(FoundationModels)
        @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
        fileprivate static func freshSession(
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
    #endif
}

private enum FMError: Error, CustomStringConvertible {
    case unavailable
    var description: String {
        switch self {
        case .unavailable: return "foundation_models_unavailable"
        }
    }
}

#if canImport(FoundationModels)

    /// Single canvas reflect tool wired into every Apple FM session.
    /// The model calls this to inspect ANY kernel agent by id.
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
    /// `sentence` from its own reflect reply.
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

    /// Aggregator: enumerate proxy_agent surfaces + host_registered.
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

    /// Write tool: create a new agent.
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelCreateAgentTool: Tool {
        let kernel: Kernel

        var name: String { "create_agent" }
        var description: String {
            """
            Create a new agent in the kernel. Returns the new \
            agent's auto-generated id on success, or {error:…} if \
            the handler_module is unknown. Common handler modules: \
            file.tools, scheduler.tools, web.tools, \
            yaml_state.tools, ollama_backend.tools.
            """
        }

        @Generable
        struct Arguments {
            @Guide(
                description:
                    "The handler_module string, e.g. 'file.tools' or 'scheduler.tools'.")
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

    /// Write tool: delete an agent (cascade).
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
