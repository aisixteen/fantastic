// SystemFMEngine — the real engine: Apple's on-device `SystemLanguageModel`.
//
// The ONLY file in the package that imports FoundationModels. Gated twice:
// `#if canImport(FoundationModels) && os(macOS)` at compile time (a Linux, iOS,
// tvOS or watchOS build compiles the refusing half only) and
// `@available(macOS 26.0, *)` at run time (an older macOS refuses, naming its
// version). Either way the refusal names the platform; nothing falls back.
//
// One generation = one FRESH `LanguageModelSession`, built from a transcript
// that replays the conversation faithfully:
//   instructions  → `Transcript.Entry.instructions` (first, only when non-empty)
//   user turn     → `Transcript.Entry.prompt`
//   assistant turn→ `Transcript.Entry.response`
// then `streamResponse(to: prompt)`. The session holds no state between
// generations — the conversation lives in the caller's memory store, and the
// caller hands it back every time.
//
// NO tools are passed and no schema is asked for: plain text in, plain text out.

import Foundation

#if canImport(FoundationModels) && os(macOS)
    import FoundationModels
#endif

#if canImport(FoundationModels) && os(macOS)
    /// The SDK's window, read once (a `static let` initialises exactly once).
    @available(macOS 26.0, *)
    private enum SDKWindow {
        static let size = SystemLanguageModel.default.contextSize
    }
#endif

public struct SystemFMEngine: FMEngine {
    public init() {}

    public func availability() -> FMAvailability {
        #if canImport(FoundationModels) && os(macOS)
            if #available(macOS 26.0, *) {
                return Self.read(SystemLanguageModel.default.availability)
            }
            let v = ProcessInfo.processInfo.operatingSystemVersion
            return .unavailable(
                reason: FMReason.platform(
                    "macOS \(v.majorVersion).\(v.minorVersion).\(v.patchVersion)"))
        #else
            return .unavailable(reason: FMReason.platform(Self.platformName))
        #endif
    }

    /// The model's window, READ from the SDK where one exists; Apple's documented
    /// figure (`FM_DOCUMENTED_CONTEXT_SIZE`) where none can be asked. The SDK
    /// getter is a synchronous XPC round trip (a semaphore wait, ~40 ms, and
    /// concurrent readers queue behind each other), so it is read ONCE per
    /// process: a getter called per reflect/validate/schema parked every
    /// cooperative thread that asked.
    public var contextSize: Int {
        #if canImport(FoundationModels) && os(macOS)
            if #available(macOS 26.0, *) {
                return SDKWindow.size
            }
        #endif
        return FM_DOCUMENTED_CONTEXT_SIZE
    }

    public var contextSizeSource: FMWindowSource {
        #if canImport(FoundationModels) && os(macOS)
            if #available(macOS 26.0, *) { return .sdk }
        #endif
        return .documented
    }

    public func stream(_ request: FMRequest) -> AsyncThrowingStream<FMEvent, Error> {
        let availability = availability()
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    try fmPreflight(availability, request)
                    try await Self.generate(request, continuation)
                    continuation.finish()
                } catch is CancellationError {
                    continuation.finish(throwing: CancellationError())
                } catch let e as FMError {
                    continuation.finish(throwing: e)
                } catch {
                    continuation.finish(throwing: FMError(fmMessage(error)))
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    // MARK: - the platform halves

    #if canImport(FoundationModels) && os(macOS)
        @available(macOS 26.0, *)
        static func read(_ a: SystemLanguageModel.Availability) -> FMAvailability {
            switch a {
            case .available:
                return .available
            case .unavailable(let why):
                switch why {
                case .deviceNotEligible: return .unavailable(reason: FMReason.deviceNotEligible)
                case .appleIntelligenceNotEnabled:
                    return .unavailable(reason: FMReason.appleIntelligenceNotEnabled)
                case .modelNotReady: return .unavailable(reason: FMReason.modelNotReady)
                @unknown default:
                    return .unavailable(reason: FMReason.unknown(String(describing: why)))
                }
            @unknown default:
                return .unavailable(reason: FMReason.unknown(String(describing: a)))
            }
        }

        /// The transcript the session is rebuilt from — instructions first (when
        /// any), then every prior turn in order.
        @available(macOS 26.0, *)
        static func transcript(_ request: FMRequest) -> Transcript {
            var entries: [Transcript.Entry] = []
            if !request.instructions.isEmpty {
                entries.append(
                    .instructions(
                        Transcript.Instructions(
                            segments: [.text(Transcript.TextSegment(content: request.instructions))],
                            toolDefinitions: [])))
            }
            for turn in request.turns {
                let segments: [Transcript.Segment] = [
                    .text(Transcript.TextSegment(content: turn.content))
                ]
                // `validate` admitted only the two roles.
                if turn.role == "user" {
                    entries.append(.prompt(Transcript.Prompt(segments: segments)))
                } else {
                    entries.append(.response(Transcript.Response(assetIDs: [], segments: segments)))
                }
            }
            return Transcript(entries: entries)
        }

        private static func generate(
            _ request: FMRequest, _ continuation: AsyncThrowingStream<FMEvent, Error>.Continuation
        ) async throws {
            guard #available(macOS 26.0, *) else {
                // `availability()` refused this already; unreachable on a
                // supported OS.
                throw FMError(fmUnavailableMessage(FMReason.platform("macOS < 26")))
            }
            let session = LanguageModelSession(
                model: SystemLanguageModel.default, tools: [], transcript: transcript(request))
            let options = GenerationOptions(
                temperature: request.temperature, maximumResponseTokens: request.maxTokens)
            var deltas = FMDeltas()
            var promptTokens: Int? = nil
            for try await snapshot in session.streamResponse(to: request.prompt, options: options) {
                if let delta = try deltas.next(snapshot.content) {
                    continuation.yield(.text(delta))
                }
                if #available(macOS 27.0, *) {
                    promptTokens = snapshot.usage.input.totalTokenCount
                }
            }
            // A CANCELLED session's stream ENDS rather than throws. Reported as the cancellation it is, never as a short
            // answer.
            if Task.isCancelled { throw CancellationError() }
            if let promptTokens { continuation.yield(.promptTokens(promptTokens)) }
        }
    #else
        private static func generate(
            _ request: FMRequest, _ continuation: AsyncThrowingStream<FMEvent, Error>.Continuation
        ) async throws {
            throw FMError(fmUnavailableMessage(FMReason.platform(platformName)))
        }

        /// What this build is, for the refusal.
        static var platformName: String {
            #if os(Linux)
                return "Linux"
            #elseif os(iOS)
                return "iOS"
            #elseif os(visionOS)
                return "visionOS"
            #elseif os(tvOS)
                return "tvOS"
            #elseif os(watchOS)
                return "watchOS"
            #elseif os(Windows)
                return "Windows"
            #elseif os(macOS)
                return "macOS without the FoundationModels SDK"
            #else
                return "an unknown platform"
            #endif
        }
    #endif
}
