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
//
// RAW tool-calling: Fantastic NEVER uses Apple's native `@Generable` Tool
// loop. The session is built with NO tools; the model GENERATES plain text
// (the `<tool_call>` envelope is taught in the instructions), and ai-core's
// shared parser extracts the call + drives the agentic loop itself.

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

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
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
            // RAW: NO native tools. The model generates plain text; the
            // `<tool_call>` envelope is taught in the instructions and parsed
            // by ai-core, which drives the agentic loop (kernel.send) itself.
            _ = kernel  // no longer wired into a native Tool
            if inst.isEmpty {
                return LanguageModelSession()
            }
            return LanguageModelSession(instructions: { Instructions(inst) })
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
