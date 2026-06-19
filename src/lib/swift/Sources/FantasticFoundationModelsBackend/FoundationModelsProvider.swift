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
    let temperature: Double
    let kernel: Kernel

    var model: String { "apple_system_language_model" }

    func chat(messages: [JSON], tools: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        // The shared core supplies the assembled messages: a `system`
        // turn (substrate primer + agent menu + send how-to + this
        // backend's always-inject memory) and — in stateless mode — the
        // single `user` turn. Map system → Apple session instructions,
        // the last user message → the prompt.
        let instructions =
            messages
            .filter { $0["role"].asString == "system" }
            .compactMap { $0["content"].asString }
            .joined(separator: "\n\n")
        let userText =
            messages.last(where: { $0["role"].asString == "user" })?["content"].asString ?? ""
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
            // ONE universal tool, matching the Python/Rust `send` — it
            // reaches EVERY agent + verb, so capability is discovered
            // (via the assembled menu + reflect), not hardcoded. Apple's
            // session runs the tool loop internally; each call routes
            // through `kernel.send`.
            let tools: [any Tool] = [KernelSendTool(kernel: kernel)]
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

    /// The ONE universal tool wired into every Apple FM session — the
    /// native twin of the Python/Rust `send(target_id, payload)`. It
    /// reaches EVERY agent and EVERY verb, so the model discovers
    /// capability from the assembled menu + reflect instead of a fixed
    /// toolset. `payload` is a JSON object string the model composes
    /// (Apple `@Generable` can't express arbitrary nested objects, so
    /// the verb + args ride as text, exactly like an OpenAI tool-call's
    /// `arguments`).
    @available(macOS 26.0, iOS 26.0, visionOS 26.0, *)
    private struct KernelSendTool: Tool {
        let kernel: Kernel

        var name: String { "send" }
        var description: String {
            """
            Send a message to ANY agent in the kernel — the single \
            universal action. Every agent answers reflect (identity + \
            verbs). Discover agents by sending {"type":"list_agents"} to \
            'core'; read the whole-system guide with \
            {"type":"reflect","readme":true} to 'core'. NEVER invent \
            agent ids — reflect first. NEVER claim "no access" without \
            trying.
            """
        }

        @Generable
        struct Arguments {
            @Guide(description: "Agent id to send to, e.g. 'core', 'cli', 'fm'.")
            let targetId: String
            @Guide(
                description:
                    "JSON object string with the verb + args, e.g. {\"type\":\"reflect\"} or {\"type\":\"list_agents\"}."
            )
            let payload: String
        }

        func call(arguments: Arguments) async throws -> String {
            let target = arguments.targetId.trimmingCharacters(in: .whitespaces)
            if target.isEmpty {
                return #"{"error":"empty target_id"}"#
            }
            let raw = arguments.payload.trimmingCharacters(in: .whitespaces)
            let payloadJSON: JSON
            if raw.isEmpty {
                payloadJSON = .object(["type": .string("reflect")])
            } else if let parsed = try? JSON.parse(raw), parsed.asObject != nil {
                payloadJSON = parsed
            } else {
                return
                    #"{"error":"payload must be a JSON object string like {\"type\":\"reflect\"}"}"#
            }
            let reply = await kernel.send(AgentId(target), payloadJSON)
            return reply.serialize()
        }
    }

#endif
