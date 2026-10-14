// Apple Foundation Models LLM backend — the on-device system model.
//
// `foundation_models_backend` lives in BOTH swift and
// rust, ONE implementation of the FoundationModels calls (`FantasticFMCore`):
// this bundle calls it directly, rust links it through `FantasticFMBridge`'s C
// ABI. Python has none (Apple-only native), so parity is swift <-> rust: the
// same bundle name, record schema, reflect shape and refusal texts, byte for
// byte. The readme both serve is ONE file (`Resources/readme.md`).
//
// On-device: no endpoint, no model choice, no credentials — a record naming one
// is refused at the write rather than shown in reflect as if it meant
// something.
//
// RAW tool-calling, like every backend: the provider streams raw text and
// `ai_core` owns the `<call>` grammar. Never FM's Tool protocol, never guided
// generation.
//
// NO FALLBACKS: an unavailable model fails the turn LOUD, naming the reason
// (device not eligible / Apple Intelligence not enabled / model not ready /
// platform not macOS 26+). Nothing is routed to another backend.
//
// THE WINDOW is the model's own, read from the SDK (`contextSize`): it is the
// `context_window` default in the schema and in the context machinery
// (`AIBackendConfig.modelWindow`), and a record may only name a smaller one.

import FantasticAICore
import FantasticFMCore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "foundation_models_backend.tools"

let NAME = "foundation_models_backend"

/// The fields an on-device record must NOT carry — there is nothing for them to
/// mean here, and reflect shows `model`/`endpoint` straight off the record.
let OFF_DEVICE_FIELDS = ["endpoint", "model", "credentials"]

public final class FoundationModelsBackendBundle: AgentBundle, @unchecked Sendable {
    public let name = NAME
    public var verbs: [String] { AIBackend.verbNames }
    public var verbDocTable: JSON { .object(AIBackend.VERBS) }
    public var emitsDoc: JSON? { .object(AIBackend.REFLECT_EMITS) }
    public var cancelDoc: JSON? { .string(CANCEL_DOC) }
    public var notesDoc: JSON? { .object(AIBackend.NOTES) }

    let engine: any FMEngine
    let core: AIBackend

    /// `engine` is the seam: the system model in a kernel, a fake in the unit
    /// suite.
    public init(engine: any FMEngine) {
        self.engine = engine
        self.core = buildAIBackend(
            AIBackendConfig(
                kind: NAME,
                sentence:
                    "Apple Foundation Models LLM agent — the on-device system model (raw "
                    + "prompt-and-parse tool-calling; no endpoint, no model choice, no key).",
                modelWindow: Int64(engine.contextSize),
                reflectExtra: { agent, _ in Self.reflectExtra(agent, engine: engine) },
                makeProvider: { agent, _, _ in try Self.makeProvider(agent, engine: engine) }
            ))
    }

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await core.handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    /// The kernel is killing a task of this agent: close the turn out for the
    /// caller; the cancellation itself tears the on-device stream down.
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        try await core.onCancel(agentId: agentId, taskId: taskId, kernel: kernel)
    }

    /// The shared delete hook: the deleted id's AI-core rows go with it.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await core.onDelete(agentId: agentId, kernel: kernel)
    }

    // MARK: - record -> provider

    /// The provider for one pass. REFUSES an unavailable model here — inside the
    /// turn, before the context is projected — so the caller reads the reason,
    /// not a window failure on a model that could never have answered.
    static func makeProvider(_ agent: Agent, engine: any FMEngine) throws(PyError)
        -> FoundationModelsProvider
    {
        if case .unavailable(let reason) = engine.availability() {
            throw PyError("RuntimeError", "\(NAME): \(fmUnavailableMessage(reason))")
        }
        return FoundationModelsProvider(
            engine: engine,
            temperature: agent.metaValue(forKey: "temperature").flatMap(number),
            maxTokens: agent.metaValue(forKey: "max_tokens")?.asInt.map { Int($0) })
    }

    /// An int or a float as a Double — a bool is neither.
    static func number(_ v: JSON) -> Double? {
        switch v {
        case .integer(let i): return Double(i)
        case .double(let d): return d
        default: return nil
        }
    }

    /// Merged into reflect AFTER the shared base: whether the model can answer
    /// (and why not), its real window, and the two knobs as the record sets them
    /// (null = the model's own).
    static func reflectExtra(_ agent: Agent, engine: any FMEngine)
        -> OrderedDictionary<String, JSON>
    {
        let a = engine.availability()
        return [
            "available": .bool(a.isAvailable),
            "unavailable_reason": a.reason.map(JSON.string) ?? .null,
            "context_size": .integer(Int64(engine.contextSize)),
            "temperature": agent.metaValue(forKey: "temperature") ?? .null,
            "max_tokens": agent.metaValue(forKey: "max_tokens") ?? .null,
        ]
    }

    // MARK: - validate_meta

    /// Refuse, AT THE WRITE, a record this backend cannot honour — each check
    /// names the field and the legal shape; the schema's own check runs last.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        for key in OFF_DEVICE_FIELDS where meta[key] != nil && meta[key] != .null {
            return "\(NAME): `\(key)` has no meaning here — the model is Apple's on-device "
                + "system model (no endpoint, no model choice, no credentials)"
        }
        if let v = meta["temperature"], v != .null {
            guard let t = Self.number(v), t.isFinite, t >= 0 else {
                return "temperature must be a number >= 0"
            }
        }
        if let v = meta["max_tokens"], v != .null {
            guard case .integer(let n) = v, n >= 1 else { return "max_tokens must be a positive int" }
        }
        if let v = meta["context_window"], v != .null {
            let size = Int64(engine.contextSize)
            guard case .integer(let n) = v, n >= 1, n <= size else {
                return "context_window must be a positive int no larger than the on-device "
                    + "model's window (\(size) tokens)"
            }
        }
        return RecordSchema.validate(
            recordSchema ?? .null, backend: NAME, meta: meta, record: record)
    }

    /// THE RECORD SCHEMA — its own two knobs, then the shared AI fields, with
    /// `context_window` defaulting to the model's real window. Neither knob has a
    /// default here: unset, the model uses its own.
    public var recordSchema: JSON? { Self.schema(contextWindow: Int64(engine.contextSize)) }

    static func schema(contextWindow: Int64) -> JSON {
        RecordSchema.schema(
            [
                RecordSchema.field(
                    "temperature", "float",
                    "sampling temperature (>= 0); unset, the on-device model's own"),
                RecordSchema.field(
                    "max_tokens", "int",
                    "a turn's output budget (tokens); unset, the on-device model's own"),
            ], contextWindow: contextWindow)
    }

    // MARK: - the readme

    /// `Resources/readme.md` — the ONE file both runtimes serve (rust includes
    /// the same bytes). Persisted as `agents/<id>/readme.md` and served by
    /// `reflect readme=true`.
    static let readmeText: String = {
        guard let url = Bundle.module.url(forResource: "readme", withExtension: "md"),
            let text = try? String(contentsOf: url, encoding: .utf8)
        else {
            fatalError("\(NAME): Resources/readme.md is missing from the resource bundle")
        }
        return text
    }()
}

// MARK: - the provider

/// `ai_core`'s chat messages -> one FMCore request, streamed as RAW TEXT.
///
/// THE SPLIT (rust mirrors it exactly):
///   * every `system` message -> `instructions`, joined with a blank line, in order;
///   * the LAST non-system message must be `user` -> `prompt`;
///   * every other non-system message, in order -> `turns` (`user`/`assistant`).
struct FoundationModelsProvider: AIProvider {
    let engine: any FMEngine
    let temperature: Double?
    let maxTokens: Int?

    var model: String { "SystemLanguageModel.default" }

    /// The model's own count of the last prompt (macOS 27+), for `ai_core`'s
    /// calibration. FM reports no stop reason, so none is claimed.
    let usage: AIUsage? = AIUsage()

    /// The request for `messages`, or the refusal for a list the session cannot
    /// answer.
    static func request(_ messages: [JSON], temperature: Double?, maxTokens: Int?)
        throws(PyError) -> FMRequest
    {
        func content(_ m: JSON) -> String {
            guard let obj = m.asObject, let v = obj["content"] else { return "" }
            return pyStr(v)
        }
        let system = messages.filter { $0["role"].asString == "system" }.map(content)
        let rest = messages.filter { $0["role"].asString != "system" }
        guard let last = rest.last, last["role"].asString == "user" else {
            let role = rest.last.map { pyStr($0["role"]) } ?? "none"
            throw PyError(
                "RuntimeError",
                "\(NAME): the last message is not a user turn (role \(role)) — the "
                    + "on-device session answers a prompt")
        }
        return FMRequest(
            instructions: system.joined(separator: "\n\n"),
            turns: rest.dropLast().map { FMTurn(role: pyStr($0["role"]), content: content($0)) },
            prompt: content(last),
            temperature: temperature,
            maxTokens: maxTokens)
    }

    func chat(messages: [JSON]) -> AsyncThrowingStream<AIChunk, Error> {
        chat(messages: messages, maxTokens: nil)
    }

    /// `sideBound` bounds a side call (a fold summary); nil, the record's
    /// `max_tokens` (or the model's own) bounds the turn.
    func chat(messages: [JSON], maxTokens sideBound: Int?) -> AsyncThrowingStream<AIChunk, Error> {
        let engine = engine
        let usage = usage
        let request: FMRequest
        do {
            request = try Self.request(
                messages, temperature: temperature, maxTokens: sideBound ?? maxTokens)
        } catch {
            return AsyncThrowingStream { $0.finish(throwing: error) }
        }
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    for try await event in engine.stream(request) {
                        switch event {
                        case .text(let delta): continuation.yield(.token(delta))
                        case .promptTokens(let n): usage?.lastPromptTokens = n
                        }
                    }
                    continuation.finish()
                } catch is CancellationError {
                    continuation.finish(throwing: CancellationError())
                } catch let e as PyError {
                    continuation.finish(throwing: e)
                } catch {
                    // FMCore's words, under this bundle's name — exactly what
                    // rust raises from the bridge's `on_done` error.
                    continuation.finish(
                        throwing: PyError("RuntimeError", "\(NAME): \(fmMessage(error))"))
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
