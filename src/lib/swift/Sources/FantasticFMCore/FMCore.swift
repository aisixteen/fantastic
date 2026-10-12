// FantasticFMCore — THE ONE implementation of the Apple Foundation Models calls.
//
// Owner's decision (2026-10-02): an on-device Foundation Models backend in BOTH
// swift and rust, with ONE implementation of the FoundationModels calls. This
// target is it. The swift bundle (`FantasticFoundationModelsBackend`) uses it
// directly; rust links it as a static library through the C ABI in
// `FantasticFMBridge`. Neither side talks to FoundationModels any other way.
//
// What it does, and nothing more:
//   * `availability()` — available, or the exact reason it is not;
//   * `contextSize` — the model's real window, read from the SDK;
//   * `stream(request)` — RAW TEXT DELTAS for one prompt over a rebuilt
//     transcript, cancellable.
//
// RAW ONLY. The session is built with NO tools and asked for plain text: never
// FM's Tool protocol, never guided generation (`@Generable`). The caller's
// `ai_core` teaches the `<call>` grammar in the instructions and parses it out
// of the text, exactly as for the ollama / vllm / responses backends.
//
// NO FALLBACKS. An unavailable model is a refusal naming the reason; nothing is
// routed anywhere else.
//
// This file is platform-neutral (Foundation only) and compiles everywhere; the
// FoundationModels calls themselves live in `SystemFMEngine.swift`, gated there.

import Foundation

// MARK: - the window

/// The on-device model's context window as Apple DOCUMENTS it, in tokens — used
/// only where no SDK can be asked (a platform without FoundationModels, or a
/// macOS older than 26, where every send is refused anyway). Wherever the SDK
/// exists the window is READ from it (`SystemLanguageModel.contextSize`).
///
/// Sources: Apple TN3193 "Managing the on-device foundation model's context
/// window" ("the on-device model ... has a context window of 4096 tokens"), and
/// the SDK itself — `SystemLanguageModel.contextSize` is `@backDeployed(before:
/// macOS 26.4)` with the body `return 4096` for the releases that predate the
/// real reading (FoundationModels.swiftinterface, MacOSX27.0.sdk). Measured on
/// this machine (macOS 27.0, M1 Pro, 2026-10-02): the SDK reads 4096.
public let FM_DOCUMENTED_CONTEXT_SIZE = 4096

/// Where a `contextSize` came from: the SDK, or Apple's documented figure.
public enum FMWindowSource: String, Sendable {
    case sdk
    case documented
}

// MARK: - availability

/// Whether the on-device model can answer right now — or the exact reason it
/// cannot, as one sentence that names it.
public enum FMAvailability: Sendable, Equatable {
    case available
    case unavailable(reason: String)

    public var isAvailable: Bool {
        if case .available = self { return true }
        return false
    }

    /// The reason, or nil when available.
    public var reason: String? {
        if case .unavailable(let r) = self { return r }
        return nil
    }

    /// `{"available":bool,"reason":str|null}` — keys in this order, no spaces.
    /// The bytes `fm_availability` hands across the C ABI.
    public var json: String {
        "{\"available\":\(isAvailable),\"reason\":\(reason.map(jsonString) ?? "null")}"
    }
}

/// The reason sentences, ONE spelling each — every refusal the swift bundle or
/// the rust bundle shows is built from these (rust reads them through
/// `fm_availability`).
public enum FMReason {
    public static let deviceNotEligible =
        "device not eligible — this device cannot run Apple Intelligence"
    public static let appleIntelligenceNotEnabled =
        "Apple Intelligence is not enabled — turn it on in System Settings > "
        + "Apple Intelligence & Siri"
    public static let modelNotReady =
        "model not ready — the on-device model is still downloading or preparing; "
        + "retry once it is ready"

    /// A reason the SDK added after this build — named as the SDK prints it.
    public static func unknown(_ described: String) -> String {
        "unavailable for a reason this build does not know: \(described)"
    }

    /// Not macOS 26 or later, naming what it is instead.
    public static func platform(_ what: String) -> String {
        "platform not macOS 26+ — this is \(what)"
    }
}

// MARK: - the request

/// One prior turn of the conversation: `user` (a prompt the model was given) or
/// `assistant` (what it answered). System text is never a turn — it is the
/// request's `instructions`.
public struct FMTurn: Sendable, Equatable {
    public var role: String
    public var content: String

    public init(role: String, content: String) {
        self.role = role
        self.content = content
    }
}

/// One generation: the instructions, the prior turns (rebuilt into the
/// session's transcript, in order), the prompt to answer, and the two options
/// the backend exposes. A nil option is the model's own default.
public struct FMRequest: Sendable, Equatable {
    public var instructions: String
    public var turns: [FMTurn]
    public var prompt: String
    public var temperature: Double?
    public var maxTokens: Int?

    public init(
        instructions: String, turns: [FMTurn], prompt: String,
        temperature: Double? = nil, maxTokens: Int? = nil
    ) {
        self.instructions = instructions
        self.turns = turns
        self.prompt = prompt
        self.temperature = temperature
        self.maxTokens = maxTokens
    }

    /// The roles a transcript turn may carry.
    public static let ROLES = ["user", "assistant"]

    /// Refuse what a session cannot take, before any model call — the same
    /// checks, and the same words, whichever runtime sent the request.
    public func validate() throws(FMError) {
        for (i, t) in turns.enumerated() where !Self.ROLES.contains(t.role) {
            throw FMError(
                "turn \(i) has role \(jsonString(t.role)) — a transcript turn is user or "
                    + "assistant (system text goes in instructions)")
        }
        if let t = temperature, !(t.isFinite && t >= 0) {
            throw FMError("temperature must be a number >= 0, got \(t)")
        }
        if let n = maxTokens, n < 1 {
            throw FMError("max_tokens must be a positive int, got \(n)")
        }
    }

    /// Parse the C ABI's `request_json`:
    /// `{"instructions":str,"turns":[{"role":str,"content":str}],"prompt":str,
    ///   "temperature":number|null?,"max_tokens":int|null?}`.
    /// `instructions`, `turns` and `prompt` are required; other keys are ignored.
    public init(json: Data) throws(FMError) {
        let wire: Wire
        do {
            wire = try JSONDecoder().decode(Wire.self, from: json)
        } catch let e as DecodingError {
            throw FMError("request_json: \(Self.describe(e))")
        } catch {
            throw FMError("request_json: \(error)")
        }
        self.init(
            instructions: wire.instructions,
            turns: wire.turns.map { FMTurn(role: $0.role, content: $0.content) },
            prompt: wire.prompt, temperature: wire.temperature, maxTokens: wire.max_tokens)
    }

    private struct Wire: Decodable {
        struct Turn: Decodable {
            let role: String
            let content: String
        }
        let instructions: String
        let turns: [Turn]
        let prompt: String
        let temperature: Double?
        let max_tokens: Int?
    }

    /// A Swift type as the JSON a sender should have written.
    private static func jsonWord(_ type: Any.Type) -> String {
        if type == String.self { return "a string" }
        if type == Int.self { return "an int" }
        if type == Double.self { return "a number" }
        let name = String(describing: type)
        if name.hasPrefix("Array") { return "a list" }
        if name.hasPrefix("Dictionary") || name.hasPrefix("KeyedDecodingContainer") {
            return "an object"
        }
        return "a \(name)"
    }

    /// A decoding failure as the path it failed at — `turns[1].role is missing`.
    private static func describe(_ e: DecodingError) -> String {
        func path(_ keys: [CodingKey]) -> String {
            var out = ""
            for k in keys {
                if let i = k.intValue { out += "[\(i)]" } else { out += (out.isEmpty ? "" : ".") + k.stringValue }
            }
            return out.isEmpty ? "the document" : "`\(out)`"
        }
        switch e {
        case .keyNotFound(let key, let ctx):
            return "\(path(ctx.codingPath + [key])) is missing"
        case .typeMismatch(let type, let ctx):
            return "\(path(ctx.codingPath)) is not \(jsonWord(type))"
        case .valueNotFound(let type, let ctx):
            return "\(path(ctx.codingPath)) is null, expected \(jsonWord(type))"
        case .dataCorrupted(let ctx):
            return "\(path(ctx.codingPath)): \(ctx.debugDescription)"
        @unknown default:
            return "\(e)"
        }
    }
}

// MARK: - the stream

/// One event off a generation, in order: text DELTAS, then — where the SDK
/// reports it (macOS 27+) — the model's own count of the prompt, once, last.
public enum FMEvent: Sendable, Equatable {
    /// New text since the last delta. Never empty.
    case text(String)
    /// The prompt as the model counted it (instructions + transcript + prompt),
    /// in tokens.
    case promptTokens(Int)
}

/// A failure, said in words. The bundles prefix their own name
/// (`foundation_models_backend: `) and raise it; the C ABI hands the message
/// across as is.
public struct FMError: Error, Sendable, Equatable, CustomStringConvertible {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// The message `fm_stream` reports when a stream was cancelled.
public let FM_CANCELLED = "cancelled"

/// The text a refused or failed generation carries. Cancellation is not a
/// failure and is never described here — callers check for it first.
public func fmMessage(_ error: Error) -> String {
    if let e = error as? FMError { return e.message }
    let localized = error.localizedDescription
    let described = String(describing: error)
    // A LanguageModelError's `description` carries the measured specifics
    // ("Content contains 7557 tokens, which exceeds the maximum allowed context
    // size of 4096."); its localized text is the category. Both, once each.
    let detail = described == localized ? localized : "\(localized) (\(described))"
    return "the on-device model failed: \(detail)"
}

/// The refusal for an unavailable model — the reason, prefixed once.
public func fmUnavailableMessage(_ reason: String) -> String {
    "the on-device model is unavailable: \(reason)"
}

/// Turns the CUMULATIVE snapshots a `LanguageModelSession` streams into DELTAS.
///
/// Compared as UTF-8: a snapshot must EXTEND what was already sent, byte for
/// byte. Comparing `Character`s would refuse a correct stream whose next byte
/// joins the last grapheme (an `e` then a combining accent). A snapshot that
/// rewrites sent text is an error — a delta already handed on cannot be taken
/// back, and pretending otherwise would corrupt the caller's text.
public struct FMDeltas: Sendable {
    private var sent: String = ""

    public init() {}

    /// The new text in `snapshot`, or nil when there is none.
    public mutating func next(_ snapshot: String) throws(FMError) -> String? {
        guard snapshot.utf8.starts(with: sent.utf8) else {
            throw FMError(
                "the on-device stream rewrote text it had already streamed "
                    + "(\(sent.utf8.count) bytes sent, the new snapshot does not extend them)")
        }
        let fresh = snapshot.utf8.count - sent.utf8.count
        guard fresh > 0 else { return nil }
        let delta = String(decoding: snapshot.utf8.suffix(fresh), as: UTF8.self)
        sent = snapshot
        return delta
    }
}

/// What every engine checks before a model call, in this order: the model is
/// available (else the refusal naming the reason), then the request is valid.
/// The real engine and the unit suite's fake both run THIS, so the refusals the
/// suite pins are the ones a kernel shows.
public func fmPreflight(_ availability: FMAvailability, _ request: FMRequest) throws(FMError) {
    if case .unavailable(let reason) = availability {
        throw FMError(fmUnavailableMessage(reason))
    }
    try request.validate()
}

// MARK: - the engine seam

/// What the backends call. `SystemFMEngine` is the real one; tests inject a
/// fake through this seam so the default suite never needs the model.
public protocol FMEngine: Sendable {
    /// Available, or the exact reason it is not.
    func availability() -> FMAvailability

    /// The model's context window in tokens, and where the number came from.
    var contextSize: Int { get }
    var contextSizeSource: FMWindowSource { get }

    /// Stream one generation as RAW TEXT DELTAS. Refuses an unavailable model
    /// and an invalid request with an `FMError`; a cancelled consumer ends the
    /// stream with `CancellationError`.
    func stream(_ request: FMRequest) -> AsyncThrowingStream<FMEvent, Error>
}

// MARK: - JSON text

/// A JSON string literal for `s`: quotes, backslashes and control characters
/// escaped, everything else as is (UTF-8). Deterministic bytes — the C ABI's
/// JSON is compared byte for byte across runtimes.
public func jsonString(_ s: String) -> String {
    var out = "\""
    for u in s.unicodeScalars {
        switch u {
        case "\"": out += "\\\""
        case "\\": out += "\\\\"
        case "\n": out += "\\n"
        case "\r": out += "\\r"
        case "\t": out += "\\t"
        case _ where u.value < 0x20:
            let hex = String(u.value, radix: 16)
            out += "\\u" + String(repeating: "0", count: 4 - hex.count) + hex
        default: out.unicodeScalars.append(u)
        }
    }
    return out + "\""
}
