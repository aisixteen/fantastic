// Per-backend configuration threaded through the shared `AIBackend`
// machinery — mirrors Rust's `fantastic-ai-core::BackendConfig`. The
// `AIProvider` (built per send by `makeProvider`) is the streaming
// seam; this struct carries the few behavioural / shape differences
// that remain between ollama, NIM, and Apple FM so a single shared
// `send`/`history`/`status`/`recall`/… serves all of them.

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
    /// Stable provider tag echoed in `reflect`,
    /// e.g. `"ollama"`, `"nvidia_nim"`, `"apple_foundation_models"`.
    public var provider: String
    /// One-line `reflect.sentence`.
    public var sentence: String

    /// NIM-only: the `done` event on the ERROR path still carries the
    /// `accumulated` field (NIM's old `emitDone` always included it).
    /// Ollama / FM omit `accumulated` from error-path `done` events.
    public var includeAccumulatedOnError: Bool

    /// Extra system-prompt text appended AFTER the shared assembly
    /// (primer + self-reflect + context how-to). The Apple-FM
    /// backend uses this for its always-inject durable memory (each
    /// memory documents' content + any custom
    /// `instructions` meta). Receives the agent + kernel so it can read
    /// state on demand; runs every turn. Default empty (ollama / NIM).
    public var systemPromptExtra: @Sendable (Agent, Kernel) async -> String

    /// A HARD, SMALL window: assemble the SHORT how-to instead of the full one,
    /// and skip the context/compaction block entirely.
    ///
    /// Set by the on-device Apple FM backend alone. Its window is a hard 4096
    /// tokens and the canonical `SEND_TO_HOWTO` is ~2100 of them, so the full
    /// block plus the primer reflect exceeded the window before the user's text
    /// was added — the session refused with `exceededContextWindowSize` and every
    /// send failed. The compaction block is dropped too: at 4096 tokens it costs
    /// more of the window than the discipline it teaches saves.
    public var compactPrompt: Bool

    /// Extra fields merged into the `reflect` reply (e.g. ollama/NIM add
    /// `host` + `model`; FM adds `available` + `model`). Receives the
    /// agent so it can read meta. Pure function — no side effects.
    public var reflectExtra: @Sendable (Agent) -> [String: JSON]

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
        compactPrompt: Bool = false,
        includeAccumulatedOnError: Bool = false,
        reflectExtra: @escaping @Sendable (Agent) -> [String: JSON] = { _ in [:] },
        systemPromptExtra: @escaping @Sendable (Agent, Kernel) async -> String = { _, _ in "" },
        makeProvider:
            @escaping @Sendable (Agent, String, Kernel) async -> ProviderResult
    ) {
        self.kind = kind
        self.provider = provider
        self.sentence = sentence
        self.compactPrompt = compactPrompt
        self.includeAccumulatedOnError = includeAccumulatedOnError
        self.reflectExtra = reflectExtra
        self.systemPromptExtra = systemPromptExtra
        self.makeProvider = makeProvider
    }
}
