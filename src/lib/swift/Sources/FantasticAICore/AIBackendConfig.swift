// Per-backend configuration threaded through the shared `AIBackend`
// machinery — mirrors Rust's `fantastic-ai-core::BackendConfig`. The
// `AIProvider` (built per send by `makeProvider`) is the streaming
// seam; this struct carries the few behavioural / shape differences
// that remain between ollama, NIM, and Apple FM so a single shared
// `send`/`history`/`status`/`recall`/… serves all of them.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Result of `makeProvider`: either a ready provider or a refusal the
/// `send` verb returns verbatim (e.g. NIM with no api_key, FM when the
/// on-device model is unavailable). The error JSON is the EXACT body
/// the backend used to return inline, preserving wire byte-identity.
public enum ProviderResult: Sendable {
    case provider(any AIProvider)
    case refused(JSON)
}

/// Per-backend knobs. Defaults match the ollama shape (the simplest
/// backend); NIM and FM override the handful of fields that differ.
public struct AIBackendConfig: Sendable {
    /// `kind` / `handler_module` short name, e.g. `"ollama_backend"`. Names the
    /// bundle in its refusals; NOT a reflect field (python has no `kind` key).
    public var kind: String
    /// One-line `reflect.sentence`.
    public var sentence: String
    /// What `reflect.model` says when the record names none — python
    /// `cfg.default_model`, usually a SENTINEL that says whose choice it is
    /// rather than a guess that reaches the network.
    public var defaultModel: String
    /// What `reflect.endpoint` says when the record names none — python
    /// `cfg.default_endpoint`.
    public var defaultEndpoint: String

    /// The per-generation wall-clock ceiling, in seconds — python
    /// `cfg.send_timeout`. A FIELD, not a constant, because python's is one: its
    /// tests patch it (`SEND_TIMEOUT = 0.05`) to watch a stalled provider time
    /// out, and a ceiling only a 180-second test can reach is a ceiling nothing
    /// pins.
    public var sendTimeout: Double

    /// Map a provider failure to a CLEAN caller-facing error, or `nil` to let it
    /// RAISE. Python `cfg.error_mapper` — nvidia's only, turning an HTTP status
    /// into `send: rate limited (429); retry in 5s` / `send: provider HTTP 503`.
    ///
    /// The distinction is the whole point: a MAPPED failure is a refusal the
    /// caller can act on and settles as a `{error, client_id}` reply; an
    /// UNMAPPED one is a bug, and the handler raises so the caller's settle says
    /// `raised: true` instead of dressing a crash up as an answer.
    public var errorMapper: (@Sendable (Error) -> String?)?

    /// Wrap the provider stream. Python `cfg.stream_wrapper` — nvidia's
    /// retry-once-on-a-pre-stream-429, which needs to EMIT while it waits, so it
    /// is handed the backend's own status emitter rather than the run state.
    public var streamWrapper:
        (
            @Sendable (any AIProvider, [JSON], AIStatusEmitter) ->
                AsyncThrowingStream<AIChunk, Error>
        )?

    /// Extra fields merged into the `reflect` reply AFTER the shared base, in
    /// the order the backend lists them — python `cfg.reflect_extra`, whose
    /// `out.update(extra)` appends new keys and leaves an existing key in place.
    /// ORDERED, because reflect is read as a LINE by the model. Receives the
    /// agent so it can read meta. Pure function — no side effects.
    public var reflectExtra: @Sendable (Agent) -> OrderedDictionary<String, JSON>

    /// Build the provider for one send. Async because FM may probe
    /// on-device availability + assemble instructions. Returns
    /// `.refused(json)` to short-circuit `send` with that exact body
    /// (api_key missing, model unavailable), or `.provider(p)` to run.
    public var makeProvider:
        @Sendable (_ agent: Agent, _ clientId: String, _ kernel: Kernel) async -> ProviderResult

    public init(
        kind: String,
        sentence: String,
        defaultModel: String,
        defaultEndpoint: String,
        sendTimeout: Double = AIBackend.envSendTimeout,
        errorMapper: (@Sendable (Error) -> String?)? = nil,
        streamWrapper: (
            @Sendable (any AIProvider, [JSON], AIStatusEmitter) ->
                AsyncThrowingStream<AIChunk, Error>
        )? = nil,
        reflectExtra: @escaping @Sendable (Agent) -> OrderedDictionary<String, JSON> = {
            _ in [:]
        },
        makeProvider:
            @escaping @Sendable (Agent, String, Kernel) async -> ProviderResult
    ) {
        self.kind = kind
        self.sentence = sentence
        self.defaultModel = defaultModel
        self.defaultEndpoint = defaultEndpoint
        self.sendTimeout = sendTimeout
        self.errorMapper = errorMapper
        self.streamWrapper = streamWrapper
        self.reflectExtra = reflectExtra
        self.makeProvider = makeProvider
    }
}

/// The backend's own `status` emitter, handed to a `streamWrapper` so a wait it
/// takes is visible as an ordinary `status` event. `detail` is an ORDERED list
/// because python's `**detail` keyword order is what the event carries.
public struct AIStatusEmitter: Sendable {
    let emit: @Sendable (String, [(String, JSON)]) async -> Void

    public init(emit: @escaping @Sendable (String, [(String, JSON)]) async -> Void) {
        self.emit = emit
    }

    public func callAsFunction(_ phase: String, _ detail: [(String, JSON)]) async {
        await emit(phase, detail)
    }
}
