// Per-backend configuration threaded through the shared `AIBackend`
// machinery — mirrors Rust's `fantastic-ai-core::BackendConfig`. The
// `AIProvider` (built per send by `makeProvider`) is the streaming
// seam; this struct carries the few behavioural / shape differences
// that remain between ollama, NIM, and Apple FM so a single shared
// `send`/`history`/`interrupt`/`backend_state` serves all three.

import FantasticJSON
import FantasticKernel
import Foundation

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
    /// `kind` / `handler_module` short name, e.g. `"ollama_backend"`.
    public var kind: String
    /// Stable provider tag echoed in `reflect` + `backend_state`,
    /// e.g. `"ollama"`, `"nvidia_nim"`, `"apple_foundation_models"`.
    public var provider: String
    /// One-line `reflect.sentence`.
    public var sentence: String
    /// `reflect.verbs` map (verb name → human description). Kept
    /// per-backend because the wording differs (SSE vs atomic call).
    public var verbs: JSON

    /// Stateless mode (Apple FM): the shared `send` does NOT feed prior
    /// history back as model context (history stays UI-only), and the
    /// per-turn user/assistant history rows carry an `id` field. When
    /// `false` (ollama / NIM), history IS the model context and rows
    /// carry no `id`.
    public var stateless: Bool

    /// NIM-only: when the provider yields finalized `.toolCall`s, splice
    /// them into the persisted assistant turn's `tool_calls` array
    /// (sorted by id). Ollama / FM never emit tool-calls today, so this
    /// is a no-op for them regardless.
    public var persistToolCalls: Bool

    /// NIM-only: the `done` event on the ERROR path still carries the
    /// `accumulated` field (NIM's old `emitDone` always included it).
    /// Ollama / FM omit `accumulated` from error-path `done` events.
    public var includeAccumulatedOnError: Bool

    /// FM-only: when an `interrupt` cancels an in-flight stream, emit a
    /// terminal `done` event with `error:"interrupted"` and do NOT
    /// persist the partial assistant turn (FM's old `runLiveStream`
    /// behaviour). When `false` (ollama / NIM), a cancelled stream
    /// simply stops reading and emits the normal success-shaped `done`
    /// with whatever it accumulated, persisting that partial turn.
    public var emitInterruptedError: Bool

    /// Encode each tool-call's `arguments` as a JSON STRING in the
    /// assistant turn fed back to the provider (OpenAI shape — NIM).
    /// When `false` (ollama / FM / Python reference), arguments stay a
    /// JSON object. Mirrors Rust's `BackendConfig.tool_args_as_json`.
    public var toolArgsAsJson: Bool

    /// Dispatch a batch of tool-calls concurrently (ollama / FM) vs
    /// serially (NIM). Either way the appended `role:tool` messages keep
    /// the model-emitted order. Mirrors Rust's `parallel_tools`.
    public var parallelTools: Bool

    /// Extra system-prompt text appended AFTER the shared assembly
    /// (primer + self-reflect + agent menu + send how-to). The Apple-FM
    /// backend uses this for its always-inject durable memory (each
    /// mounted yaml_state agent's `state_yaml` + any custom
    /// `instructions` meta). Receives the agent + kernel so it can read
    /// state on demand; runs every turn. Default empty (ollama / NIM).
    public var systemPromptExtra: @Sendable (Agent, Kernel) async -> String

    /// Extra fields merged into the `reflect` reply (e.g. ollama/NIM add
    /// `host` + `model`; FM adds `available` + `model`). Receives the
    /// agent so it can read meta. Pure function — no side effects.
    public var reflectExtra: @Sendable (Agent) -> [String: JSON]

    /// Extra fields merged into the `backend_state` reply, same
    /// contract as `reflectExtra`. The shared base always supplies
    /// `provider`; the closure adds the rest.
    public var backendStateExtra: @Sendable (Agent) -> [String: JSON]

    /// Build the provider for one send. Async because FM may probe
    /// on-device availability + assemble instructions. Returns
    /// `.refused(json)` to short-circuit `send` with that exact body
    /// (api_key missing, model unavailable), or `.provider(p)` to run.
    public var makeProvider:
        @Sendable (_ agent: Agent, _ clientId: String, _ kernel: Kernel) async -> ProviderResult

    public init(
        kind: String,
        provider: String,
        sentence: String,
        verbs: JSON,
        stateless: Bool = false,
        persistToolCalls: Bool = false,
        includeAccumulatedOnError: Bool = false,
        emitInterruptedError: Bool = false,
        toolArgsAsJson: Bool = false,
        parallelTools: Bool = true,
        reflectExtra: @escaping @Sendable (Agent) -> [String: JSON] = { _ in [:] },
        backendStateExtra: @escaping @Sendable (Agent) -> [String: JSON] = { _ in [:] },
        systemPromptExtra: @escaping @Sendable (Agent, Kernel) async -> String = { _, _ in "" },
        makeProvider:
            @escaping @Sendable (Agent, String, Kernel) async -> ProviderResult
    ) {
        self.kind = kind
        self.provider = provider
        self.sentence = sentence
        self.verbs = verbs
        self.stateless = stateless
        self.persistToolCalls = persistToolCalls
        self.includeAccumulatedOnError = includeAccumulatedOnError
        self.emitInterruptedError = emitInterruptedError
        self.toolArgsAsJson = toolArgsAsJson
        self.parallelTools = parallelTools
        self.reflectExtra = reflectExtra
        self.backendStateExtra = backendStateExtra
        self.systemPromptExtra = systemPromptExtra
        self.makeProvider = makeProvider
    }
}
