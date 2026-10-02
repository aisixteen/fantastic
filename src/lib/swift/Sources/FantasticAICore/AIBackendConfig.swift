// Per-backend configuration threaded through the shared `AIBackend`
// machinery — mirrors Rust's `fantastic-ai-core::BackendConfig`. The
// `AIProvider` (built per send by `makeProvider`) is the streaming
// seam; this struct carries the few behavioural / shape differences
// that remain between the backends so a single shared
// `send`/`history`/`status`/`recall`/… serves all of them.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Per-backend knobs. Defaults match the ollama shape (the simplest
/// backend); a backend overrides the handful of fields that differ.
public struct AIBackendConfig: Sendable {
    /// `kind` / `handler_module` short name, e.g. `"ollama_backend"`. Names the
    /// bundle in its refusals; NOT a reflect field (python has no `kind` key).
    public var kind: String
    /// One-line `reflect.sentence`.
    public var sentence: String

    /// The per-generation wall-clock ceiling, in seconds — python
    /// `cfg.send_timeout`. A FIELD, not a constant, because python's is one: its
    /// tests patch it (`SEND_TIMEOUT = 0.05`) to watch a stalled provider time
    /// out, and a ceiling only a 180-second test can reach is a ceiling nothing
    /// pins.
    public var sendTimeout: Double

    /// THE MODEL'S OWN WINDOW, in tokens — what `context_window` resolves to when
    /// the record names neither `context_window` nor `num_ctx`. A server backend
    /// cannot know its model's window and keeps `AIConstants.DEFAULT_CONTEXT_WINDOW`
    /// (the record's `context_window` is the lever); a backend that can READ the
    /// real one says so here (foundation_models_backend: the SDK's `contextSize`).
    /// Its schema's `context_window` default is the same number.
    public var modelWindow: Int64

    /// Extra fields merged into the `reflect` reply AFTER the shared base, in
    /// the order the backend lists them — python `cfg.reflect_extra`, whose
    /// `out.update(extra)` appends new keys and leaves an existing key in place.
    /// ORDERED, because reflect is read as a LINE by the model. Receives the
    /// agent so it can read meta. Pure function — no side effects.
    public var reflectExtra: @Sendable (Agent) -> OrderedDictionary<String, JSON>

    /// Build the provider for one send — python `make_provider(id, kernel) ->
    /// Provider`, inside the turn. THROWS when the record lacks a connector field
    /// it must name (`requiredMeta`) — the turn fails loud, as python's raise
    /// does. A key is never read here: the provider reads it through its
    /// `KeyReader` before each request.
    public var makeProvider:
        @Sendable (_ agent: Agent, _ clientId: String, _ kernel: Kernel) async throws -> any AIProvider

    public init(
        kind: String,
        sentence: String,
        sendTimeout: Double = AIConstants.SEND_TIMEOUT,
        modelWindow: Int64 = AIConstants.DEFAULT_CONTEXT_WINDOW,
        reflectExtra: @escaping @Sendable (Agent) -> OrderedDictionary<String, JSON> = {
            _ in [:]
        },
        makeProvider:
            @escaping @Sendable (Agent, String, Kernel) async throws -> any AIProvider
    ) {
        self.kind = kind
        self.sentence = sentence
        self.sendTimeout = sendTimeout
        self.modelWindow = modelWindow
        self.reflectExtra = reflectExtra
        self.makeProvider = makeProvider
    }
}

/// A connector field the record MUST name — an endpoint or a model is never
/// guessed. Throws, naming the field, so the turn fails loud. Python
/// `ai_core.required_meta`.
public func requiredMeta(_ agent: Agent, _ key: String, _ backend: String) throws(PyError) -> String
{
    guard case .string(let v)? = agent.metaValue(forKey: key), !pyStrip(v).isEmpty else {
        throw PyError(
            "ValueError",
            "\(backend): `\(key)` is required on the agent record — nothing is "
                + "guessed (update_agent <id> \(key)=…)")
    }
    return v
}
